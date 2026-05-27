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
    RegistryCredential, Session, SessionSecrets, SessionSpec, SessionState, SnapshotRecord,
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
        .bind(SessionState::Pending.as_str())
        .bind(&spec.image)
        .bind(harness_json)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(SessionId(id))
    }

    async fn create_session_created(
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
        .bind(SessionState::Created.as_str())
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
                   created_at, last_active_at,
                   live_disk_manifest_id, live_disk_manifest_version
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
                   created_at, last_active_at,
                   live_disk_manifest_id, live_disk_manifest_version
            FROM sessions
            WHERE status IN ('pending','active','idle')
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row::session_from_row).collect()
    }

    /// ADR 0016 Phase B commit 7 — single-query rehydration source.
    /// LEFT JOIN against snapshots to compute the effective disk
    /// manifest server-side (newer of live + latest recoverable
    /// snapshot). Avoids the N+1 query the trait default would
    /// produce.
    async fn list_active_sandboxes_on_host_with_disk_manifest(
        &self,
        host_id: HostId,
    ) -> Result<
        Vec<(
            SessionId,
            SandboxId,
            Option<engram_core::types::manifest::ManifestRef>,
        )>,
        MetaError,
    > {
        // CTE picks the LATEST recoverable snapshot per session
        // (one row per session_id, ordered by created_at DESC).
        // The outer SELECT joins it with the active-on-host
        // sessions and picks max(live, snapshot) version when both
        // share the same manifest_id; if they differ, snapshot
        // wins (mirrors `effective_resume_disk_manifest`'s
        // defensive branch).
        let rows = sqlx::query(
            r#"
            WITH latest_snap AS (
                SELECT DISTINCT ON (session_id)
                       session_id,
                       disk_manifest_id      AS snap_id,
                       disk_manifest_version AS snap_version
                FROM snapshots
                WHERE session_id IS NOT NULL
                  AND recoverable = TRUE
                  AND disk_manifest_id IS NOT NULL
                ORDER BY session_id, created_at DESC
            )
            SELECT s.id              AS session_id,
                   s.sandbox_id      AS sandbox_id,
                   s.live_disk_manifest_id      AS live_id,
                   s.live_disk_manifest_version AS live_version,
                   ls.snap_id        AS snap_id,
                   ls.snap_version   AS snap_version
            FROM sessions s
            LEFT JOIN latest_snap ls ON ls.session_id = s.id
            WHERE s.host_id    = $1
              AND s.status     = 'active'
              AND s.sandbox_id IS NOT NULL
            "#,
        )
        .bind(host_id.as_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let session: Uuid = r
                .try_get("session_id")
                .map_err(|e| MetaError::Serialization(format!("rehydrate: session_id: {e}")))?;
            let sandbox: Uuid = r
                .try_get("sandbox_id")
                .map_err(|e| MetaError::Serialization(format!("rehydrate: sandbox_id: {e}")))?;
            let live_id: Option<Uuid> = r
                .try_get("live_id")
                .map_err(|e| MetaError::Serialization(format!("rehydrate: live_id: {e}")))?;
            let live_version: Option<i64> = r
                .try_get("live_version")
                .map_err(|e| MetaError::Serialization(format!("rehydrate: live_version: {e}")))?;
            let snap_id: Option<Uuid> = r
                .try_get("snap_id")
                .map_err(|e| MetaError::Serialization(format!("rehydrate: snap_id: {e}")))?;
            let snap_version: Option<i64> = r
                .try_get("snap_version")
                .map_err(|e| MetaError::Serialization(format!("rehydrate: snap_version: {e}")))?;
            let live = match (live_id, live_version) {
                (Some(id), Some(v)) => Some(engram_core::types::manifest::ManifestRef {
                    manifest_id: id,
                    version: v as u64,
                }),
                _ => None,
            };
            let snap = match (snap_id, snap_version) {
                (Some(id), Some(v)) => Some(engram_core::types::manifest::ManifestRef {
                    manifest_id: id,
                    version: v as u64,
                }),
                _ => None,
            };
            // Mirror `effective_resume_disk_manifest` exactly.
            // Same-id → max(version). Different id → snapshot wins
            // (defensive). Both None → None (sandbox without
            // chunked-disk lineage, host skips rehydration).
            let effective = match (live, snap) {
                (None, snap) => snap,
                (Some(l), None) => Some(l),
                (Some(l), Some(s)) => {
                    if l.manifest_id == s.manifest_id && l.version > s.version {
                        Some(l)
                    } else {
                        Some(s)
                    }
                }
            };
            out.push((
                SessionId::from(session),
                SandboxId::from(sandbox),
                effective,
            ));
        }
        Ok(out)
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

    async fn transition_session(
        &self,
        id: SessionId,
        target: SessionState,
    ) -> Result<SessionState, MetaError> {
        // SELECT-then-UPDATE under a row-level lock so two concurrent
        // callers can't both validate against the same pre-state. The
        // transaction commits the UPDATE atomically with the lock
        // release.
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let row = sqlx::query(
            r#"
            SELECT status FROM sessions WHERE id = $1 FOR UPDATE
            "#,
        )
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .ok_or(MetaError::NotFound)?;
        let current_raw: String = row.try_get("status").map_err(|e| {
            MetaError::Serialization(format!("transition_session: read current: {e}"))
        })?;
        let current = row::parse_session_state_for_lib(&current_raw)?;
        // Legality check — failure surfaces as Conflict with the
        // rendered IllegalTransition (carrying both sides) so callers
        // can format an HTTP 409 body without reconstructing context.
        current.try_transition_to(target).map_err(|e| {
            tracing::warn!(
                session_id = %id,
                from = %current.as_str(),
                to = %target.as_str(),
                "rejected illegal session state transition"
            );
            MetaError::Conflict(e.to_string())
        })?;
        // ADR 0018 commit 12b: entering Evacuating resets
        // `evac_attempts` to 0 so a fresh drain (operator or
        // dead-host detector) starts the scanner's retry budget
        // clean. Folded into the same UPDATE that commits the state
        // flip so the counter and the state are always consistent.
        sqlx::query(
            r#"
            UPDATE sessions
               SET status = $2,
                   last_active_at = NOW(),
                   updated_at = NOW(),
                   evac_attempts = CASE WHEN $2 = 'evacuating' THEN 0 ELSE evac_attempts END
             WHERE id = $1
            "#,
        )
        .bind(id.as_uuid())
        .bind(target.as_str())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(current)
    }

    /// ADR 0018 commit 12b: scanner sweep query. Indexed via the
    /// partial `idx_sessions_evacuating` from migration 0037 so the
    /// cost stays flat as the global session row count grows.
    async fn list_evacuating_sessions(&self) -> Result<Vec<(Session, u32)>, MetaError> {
        let rows = sqlx::query(
            r#"
            SELECT id, user_id, status, host_id, sandbox_id,
                   image_uri, harness,
                   created_at, last_active_at,
                   live_disk_manifest_id, live_disk_manifest_version,
                   evac_attempts
            FROM sessions
            WHERE status = 'evacuating'
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let session = row::session_from_row(r)?;
            let attempts: i32 = r
                .try_get("evac_attempts")
                .map_err(|e| MetaError::Serialization(format!("evac_attempts: {e}")))?;
            out.push((session, attempts.max(0) as u32));
        }
        Ok(out)
    }

    /// ADR 0018 commit 12b: atomic `+= 1 RETURNING`. Scanner calls
    /// this before each resume attempt so the returned count is
    /// the scanner's "this is my Nth try" view; when it crosses
    /// the budget threshold, the scanner falls back to Idle.
    async fn bump_evac_attempts(&self, session_id: SessionId) -> Result<u32, MetaError> {
        let row = sqlx::query(
            r#"
            UPDATE sessions
               SET evac_attempts = evac_attempts + 1
             WHERE id = $1
             RETURNING evac_attempts
            "#,
        )
        .bind(session_id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or(MetaError::NotFound)?;
        let attempts: i32 = row
            .try_get("evac_attempts")
            .map_err(|e| MetaError::Serialization(format!("bump_evac_attempts: {e}")))?;
        Ok(attempts.max(0) as u32)
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
        // ADR 0016 Phase B: the `live_disk_manifest_*` invariant is
        // "set IFF the session is bound to a running sandbox the
        // host-side FlushScheduler is publishing for." Unbinding
        // (sandbox_id = NULL) means the live manifest is no longer
        // authoritative — the snapshot row (if any) is. Clear in the
        // same UPDATE so:
        //   1. Resume's `effective_resume_disk_manifest` resolver
        //      (commit 6) sees NULL and falls back to the snapshot's
        //      `disk_manifest`. Without this, an eviction race
        //      (scheduler publishes between `host.snapshot()` and
        //      `assign_session_sandbox(None)`) would leave a live
        //      manifest AHEAD of the snapshot, producing an
        //      incoherent (memory at T-from-snapshot, disk at T+delta)
        //      resume.
        //   2. Phase C's pin set (which keys on `live_disk_manifest_id`
        //      WHERE NOT NULL) drops the post-eviction lineage from
        //      its live set so its chunks become GC-eligible after
        //      the snapshot's chunks supersede them.
        //
        // Rebinding (sandbox_id = Some) does NOT clear — the next
        // scheduler flush of the new sandbox populates the columns;
        // any leftover value from a prior binding is overwritten by
        // that publish (or the sandbox_id guard drops it as stale).
        let n = if sandbox_id.is_some() {
            sqlx::query("UPDATE sessions SET sandbox_id = $2, updated_at = NOW() WHERE id = $1")
                .bind(id.as_uuid())
                .bind(sandbox_id.map(|s| s.as_uuid()))
                .execute(&self.pool)
                .await
                .map_err(db_err)?
                .rows_affected()
        } else {
            // Single TX: clear sandbox + live manifest, AND bump
            // chunk_generation in the same step so Phase C's mid-
            // sweep barrier observes the pin-set shrink atomically.
            let mut tx = self.pool.begin().await.map_err(db_err)?;
            let n = sqlx::query(
                "UPDATE sessions
                    SET sandbox_id                 = NULL,
                        live_disk_manifest_id      = NULL,
                        live_disk_manifest_version = NULL,
                        live_disk_manifest_at      = NULL,
                        updated_at                 = NOW()
                  WHERE id = $1",
            )
            .bind(id.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?
            .rows_affected();
            // Only bump generation when we actually cleared a row
            // that had a live manifest. A NULL→NULL clear is a no-op
            // for the pin set; bumping anyway is harmless (a wasted
            // sweep restart) but the conditional keeps generation
            // bumps tied to real pin-set deltas.
            if n > 0 {
                sqlx::query(
                    "UPDATE chunk_generation SET generation = generation + 1 WHERE id = TRUE",
                )
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            }
            tx.commit().await.map_err(db_err)?;
            n
        };
        if n == 0 {
            return Err(MetaError::NotFound);
        }
        Ok(())
    }

    async fn host_for_sandbox(
        &self,
        sandbox_id: SandboxId,
    ) -> Result<Option<(HostId, SessionState)>, MetaError> {
        // ADR 0015 M3: PG-authoritative read for the
        // sandbox→host→session-status triple. Single-row, indexed on
        // `sessions.sandbox_id` (added in the routing-rebuild work in
        // ADR 0009). LIMIT 1 is paranoia — the column is logically
        // unique while bound, but the schema doesn't enforce it.
        let row: Option<(uuid::Uuid, String)> = sqlx::query_as(
            r#"
            SELECT host_id, status FROM sessions
            WHERE sandbox_id = $1 AND host_id IS NOT NULL
            LIMIT 1
            "#,
        )
        .bind(sandbox_id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        match row {
            None => Ok(None),
            Some((host_uuid, status)) => Ok(Some((
                HostId(host_uuid),
                row::parse_session_state_for_lib(&status)?,
            ))),
        }
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

    async fn mark_host_dead_and_orphan_sessions(
        &self,
        host_id: HostId,
    ) -> Result<Vec<(SessionId, SessionState)>, MetaError> {
        // ADR 0015 M2: orphaned sessions move to `host_lost`, not
        // straight to `dead`. The caller runs a per-session snapshot
        // check and drives `HostLost -> {Idle, Dead}` as a separate
        // transition. Splitting the two stages keeps "host went away"
        // a distinct lifecycle moment from "session is unrecoverable."
        //
        // RETURNING `id, prev_status` so the caller can emit honest
        // StatusChanged events. The `prev_status` is read inside the
        // same UPDATE via a CTE so we don't race with a concurrent
        // transition_session on the same row — the row is locked for
        // the duration of this transaction.
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        sqlx::query(r#"UPDATE hosts SET status = 'dead', updated_at = NOW() WHERE id = $1"#)
            .bind(host_id.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;

        let rows = sqlx::query(
            r#"
            WITH prior AS (
                SELECT id, status AS prev_status
                  FROM sessions
                 WHERE host_id = $1
                   AND status NOT IN ('completed','failed','dead')
                   FOR UPDATE
            )
            UPDATE sessions s
               SET host_id    = NULL,
                   sandbox_id = NULL,
                   status     = 'host_lost',
                   last_active_at = NOW()
              FROM prior p
             WHERE s.id = p.id
            RETURNING s.id, p.prev_status
            "#,
        )
        .bind(host_id.as_uuid())
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;

        tx.commit().await.map_err(db_err)?;

        rows.iter()
            .map(|r| {
                let uuid: uuid::Uuid = sqlx::Row::try_get(r, "id").map_err(db_err)?;
                let raw: String = sqlx::Row::try_get(r, "prev_status").map_err(db_err)?;
                let prev = row::parse_session_state_for_lib(&raw)?;
                Ok((SessionId::from(uuid), prev))
            })
            .collect::<Result<Vec<_>, MetaError>>()
    }

    async fn record_snapshot(&self, snap: SnapshotRecord) -> Result<(), MetaError> {
        // ADR 0007: single-tier durability. Every snapshot row
        // references chunked manifests in `BlobStorage` via the
        // `disk_manifest_*` / `memory_manifest_*` quartet. The
        // previous hot-tier (`local_path`) + cold-tier (envelope-
        // encrypted blob ref) columns retired with Phase 7
        // (migration 0020).
        //
        // ADR 0016 Phase C: bump `chunk_generation` in the same TX
        // so the GC barrier sees the new pin-set entry atomically
        // with the row write. Without this, a sweep that read the
        // pin set before the row committed would miss the snapshot's
        // chunks; with the bump, the sweep's post-collection
        // generation read catches the divergence and restarts.
        let mut tx = self.pool.begin().await.map_err(db_err)?;
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
        .bind(snap.session_id.map(|s| s.as_uuid()))
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
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query("UPDATE chunk_generation SET generation = generation + 1 WHERE id = TRUE")
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
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

    async fn get_snapshot(
        &self,
        id: engram_core::types::SnapshotId,
    ) -> Result<Option<SnapshotRecord>, MetaError> {
        let row = sqlx::query(
            r#"
            SELECT id, session_id, host_id,
                   image_version, size_bytes,
                   created_at, last_accessed_at,
                   disk_manifest_id, disk_manifest_version,
                   memory_manifest_id, memory_manifest_version,
                   recoverable
            FROM snapshots WHERE id = $1
            "#,
        )
        .bind(id.as_uuid())
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
        // ADR 0016 Phase C: bump `chunk_generation` in the same TX
        // as the row write so a GC sweep that collected the pin
        // set before the row committed observes the divergence at
        // its post-collection generation read and restarts. The
        // chunks themselves are materialized to BlobStorage by
        // separate `materialize_disk_chunks` code BEFORE this
        // method is called; the window between materialize and
        // row-insert is what the barrier closes.
        //
        // Commit 3a additionally persists `disk_manifest_{id,version}`
        // — the bake's ManifestRef, parsed from bundle.json — so the
        // Phase C pin-set query is a pure PG SELECT (no OCI re-pull
        // per sweep).
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        sqlx::query(
            r#"
            INSERT INTO enabled_images
                (id, image_uri, manifest_toml, manifest_digest,
                 disk_manifest_id, disk_manifest_version, base_snapshot_id,
                 last_refreshed_at, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NULL)
            ON CONFLICT (image_uri) DO UPDATE SET
                manifest_toml         = EXCLUDED.manifest_toml,
                manifest_digest       = EXCLUDED.manifest_digest,
                disk_manifest_id      = EXCLUDED.disk_manifest_id,
                disk_manifest_version = EXCLUDED.disk_manifest_version,
                base_snapshot_id      = EXCLUDED.base_snapshot_id,
                last_refreshed_at     = EXCLUDED.last_refreshed_at,
                updated_at            = NOW()
            "#,
        )
        .bind(image.id)
        .bind(&image.image_uri)
        .bind(&image.manifest_toml)
        .bind(&image.manifest_digest)
        .bind(image.disk_manifest.map(|m| m.manifest_id))
        .bind(image.disk_manifest.map(|m| m.version as i64))
        .bind(image.base_snapshot_id.map(|s| s.as_uuid()))
        .bind(image.last_refreshed_at)
        .bind(image.created_at)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query("UPDATE chunk_generation SET generation = generation + 1 WHERE id = TRUE")
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn list_enabled_images(&self) -> Result<Vec<EnabledImage>, MetaError> {
        let rows = sqlx::query(
            r#"
            SELECT id, image_uri, manifest_toml, manifest_digest,
                   disk_manifest_id, disk_manifest_version, base_snapshot_id,
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
                   disk_manifest_id, disk_manifest_version, base_snapshot_id,
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

    // ----------------------------------------------------------------
    // ADR 0016 §A.1.5c — eviction_inflight leasing row.
    // ----------------------------------------------------------------

    async fn try_acquire_eviction_lease(
        &self,
        session_id: SessionId,
        sandbox_id: engram_core::SandboxId,
        locked_by: &str,
    ) -> Result<bool, MetaError> {
        // ON CONFLICT (session_id) DO NOTHING returns 0 rows
        // affected when the row already exists. Atomic vs. a
        // racing INSERT from another coord pod.
        let res = sqlx::query(
            "INSERT INTO eviction_inflight (session_id, locked_by, sandbox_id) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (session_id) DO NOTHING",
        )
        .bind(session_id.as_uuid())
        .bind(locked_by)
        .bind(sandbox_id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() == 1)
    }

    async fn release_eviction_lease(&self, session_id: SessionId) -> Result<(), MetaError> {
        sqlx::query("DELETE FROM eviction_inflight WHERE session_id = $1")
            .bind(session_id.as_uuid())
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        // Idempotent: missing row = already released / never held.
        Ok(())
    }

    async fn sweep_stale_eviction_leases(
        &self,
        max_age: std::time::Duration,
    ) -> Result<Vec<engram_core::traits::StaleEvictionLease>, MetaError> {
        // DELETE ... RETURNING is one statement; rows lifted to
        // app-side for warn-logging. Interval is passed as seconds
        // (BIGINT-castable) because the sqlx postgres driver doesn't
        // bind `std::time::Duration` natively.
        let max_age_secs = max_age.as_secs() as i64;
        let rows = sqlx::query_as::<
            _,
            (
                uuid::Uuid,
                uuid::Uuid,
                String,
                chrono::DateTime<chrono::Utc>,
            ),
        >(
            "DELETE FROM eviction_inflight \
             WHERE locked_at < now() - make_interval(secs => $1::double precision) \
             RETURNING session_id, sandbox_id, locked_by, locked_at",
        )
        .bind(max_age_secs as f64)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|(session_id, sandbox_id, locked_by, locked_at)| {
                engram_core::traits::StaleEvictionLease {
                    session_id: SessionId::from(session_id),
                    sandbox_id: engram_core::SandboxId::from(sandbox_id),
                    locked_by,
                    locked_at,
                }
            })
            .collect())
    }

    /// ADR 0016 Phase B: publish the host's freshly-flushed disk
    /// manifest. Single TX:
    ///
    /// 1. `UPDATE sessions SET live_disk_manifest_* WHERE id = $1
    ///    AND sandbox_id = $2`. The sandbox_id guard is the core
    ///    correctness invariant — a stale publish from a destroyed
    ///    or rebound sandbox can't clobber a fresh binding.
    /// 2. If `rows_affected == 0`: return `DroppedStale` and let
    ///    the transaction roll back. No `chunk_generation` bump
    ///    because nothing was pinned.
    /// 3. Else: bump `chunk_generation` so Phase C's mid-sweep
    ///    barrier observes the new pin set atomically with the
    ///    `sessions` row write. Return `Applied`.
    async fn update_live_disk_manifest(
        &self,
        session_id: SessionId,
        sandbox_id: SandboxId,
        manifest_ref: engram_core::types::manifest::ManifestRef,
    ) -> Result<engram_core::traits::UpdateOutcome, MetaError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let rows_affected = sqlx::query(
            "UPDATE sessions
                SET live_disk_manifest_id      = $3,
                    live_disk_manifest_version = $4,
                    live_disk_manifest_at      = now()
              WHERE id         = $1
                AND sandbox_id = $2",
        )
        .bind(session_id.as_uuid())
        .bind(sandbox_id.as_uuid())
        .bind(manifest_ref.manifest_id)
        .bind(manifest_ref.version as i64)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        if rows_affected == 0 {
            // Rollback the TX explicitly — nothing was written, but
            // letting it implicit-drop is fine; this is documentation.
            tx.rollback().await.map_err(db_err)?;
            return Ok(engram_core::traits::UpdateOutcome::DroppedStale);
        }
        sqlx::query("UPDATE chunk_generation SET generation = generation + 1 WHERE id = TRUE")
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(engram_core::traits::UpdateOutcome::Applied)
    }

    /// ADR 0016 Phase C: read the one-row `chunk_generation` counter.
    /// Sweep before-and-after read; mid-sweep restart on bump.
    async fn chunk_generation(&self) -> Result<u64, MetaError> {
        let row =
            sqlx::query_scalar::<_, i64>("SELECT generation FROM chunk_generation WHERE id = TRUE")
                .fetch_one(&self.pool)
                .await
                .map_err(db_err)?;
        Ok(row as u64)
    }

    /// ADR 0016 Phase C: explicit barrier bump for non-flush manifest-
    /// lineage writes (`enable_image`, `record_snapshot`). Phase B's
    /// `update_live_disk_manifest` already bumps inline in its own
    /// TX; this method lets callers that don't share that TX (or
    /// don't want to refactor their existing TX boundary) tick the
    /// generation in a one-shot statement after their own write
    /// commits.
    async fn bump_chunk_generation(&self) -> Result<(), MetaError> {
        sqlx::query("UPDATE chunk_generation SET generation = generation + 1 WHERE id = TRUE")
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// ADR 0016 Phase C: pin-set source #3 — every recoverable
    /// snapshot's chunked-disk `ManifestRef`. Filtered to
    /// `recoverable=true`; the `idx_snapshots_disk_manifest`
    /// partial index (migration 0018) plus the `recoverable` boolean
    /// filter combine via PG's planner to scan only recoverable
    /// chunked rows.
    async fn list_recoverable_snapshot_disk_manifests(
        &self,
    ) -> Result<Vec<engram_core::types::manifest::ManifestRef>, MetaError> {
        let rows = sqlx::query_as::<_, (Uuid, i64)>(
            "SELECT DISTINCT disk_manifest_id, disk_manifest_version
               FROM snapshots
              WHERE disk_manifest_id IS NOT NULL
                AND disk_manifest_version IS NOT NULL
                AND recoverable = TRUE",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|(id, version)| engram_core::types::manifest::ManifestRef {
                manifest_id: id,
                version: version as u64,
            })
            .collect())
    }

    /// ADR 0016 Phase C: pin-set source #4 — memory-side mirror.
    /// `idx_snapshots_memory_manifest` (migration 0019) +
    /// `recoverable=TRUE` filter.
    async fn list_recoverable_snapshot_memory_manifests(
        &self,
    ) -> Result<Vec<engram_core::types::manifest::ManifestRef>, MetaError> {
        let rows = sqlx::query_as::<_, (Uuid, i64)>(
            "SELECT DISTINCT memory_manifest_id, memory_manifest_version
               FROM snapshots
              WHERE memory_manifest_id IS NOT NULL
                AND memory_manifest_version IS NOT NULL
                AND recoverable = TRUE",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|(id, version)| engram_core::types::manifest::ManifestRef {
                manifest_id: id,
                version: version as u64,
            })
            .collect())
    }

    /// ADR 0016 Phase C: pin-set source #1 — every enabled image's
    /// chunked-disk base manifest. The partial index
    /// `idx_enabled_images_disk_manifest` (migration 0036) skips
    /// harness-only rows (`disk_manifest_id IS NULL`) so the scan
    /// is bounded by chunked-image count.
    async fn list_enabled_image_disk_manifest_ids(
        &self,
    ) -> Result<Vec<engram_core::types::manifest::ManifestRef>, MetaError> {
        let rows = sqlx::query_as::<_, (Uuid, i64)>(
            "SELECT DISTINCT disk_manifest_id, disk_manifest_version
               FROM enabled_images
              WHERE disk_manifest_id IS NOT NULL
                AND disk_manifest_version IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|(id, version)| engram_core::types::manifest::ManifestRef {
                manifest_id: id,
                version: version as u64,
            })
            .collect())
    }

    /// ADR 0016 Phase C: pin-set source #2 — every session that has
    /// published a live disk manifest. The partial index
    /// `idx_sessions_live_disk_manifest` (migration 0034) covers the
    /// predicate so the scan is bounded by live-manifest count, not
    /// total session count.
    async fn list_live_session_disk_manifest_ids(
        &self,
    ) -> Result<Vec<engram_core::types::manifest::ManifestRef>, MetaError> {
        let rows = sqlx::query_as::<_, (Uuid, i64)>(
            "SELECT DISTINCT live_disk_manifest_id, live_disk_manifest_version
               FROM sessions
              WHERE live_disk_manifest_id IS NOT NULL
                AND live_disk_manifest_version IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|(id, version)| engram_core::types::manifest::ManifestRef {
                manifest_id: id,
                version: version as u64,
            })
            .collect())
    }

    /// ADR 0016 Phase C: idempotent candidate upsert. ON CONFLICT
    /// refreshes `last_seen_at` only — `first_seen_at` is sticky so
    /// the grace window starts when the candidate first appeared,
    /// not when it was last re-seen.
    async fn upsert_chunk_gc_candidate(&self, hash: [u8; 32]) -> Result<(), MetaError> {
        sqlx::query(
            "INSERT INTO chunk_gc_candidates (content_hash)
             VALUES ($1)
             ON CONFLICT (content_hash) DO UPDATE SET last_seen_at = now()",
        )
        .bind(hash.as_slice())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// ADR 0016 Phase C promote-pass query.
    async fn list_expired_gc_candidates(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
        limit: i64,
    ) -> Result<Vec<[u8; 32]>, MetaError> {
        let rows = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT content_hash
               FROM chunk_gc_candidates
              WHERE first_seen_at < $1
              ORDER BY first_seen_at
              LIMIT $2",
        )
        .bind(cutoff)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for bytes in rows {
            if bytes.len() != 32 {
                // A non-32-byte row is a data corruption — fail
                // loud rather than silently truncate.
                return Err(MetaError::Serialization(format!(
                    "chunk_gc_candidates.content_hash has {} bytes, expected 32",
                    bytes.len()
                )));
            }
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&bytes);
            out.push(arr);
        }
        Ok(out)
    }

    /// ADR 0016 Phase C: paged read of the candidate table for the
    /// admin GET endpoint. ORDER BY first_seen_at ASC matches the
    /// promote-pass shape so operators see the oldest backlog
    /// first.
    async fn list_gc_candidates(
        &self,
        limit: i64,
        before: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<engram_core::traits::GcCandidateRow>, MetaError> {
        let rows = if let Some(cutoff) = before {
            sqlx::query_as::<
                _,
                (
                    Vec<u8>,
                    chrono::DateTime<chrono::Utc>,
                    chrono::DateTime<chrono::Utc>,
                ),
            >(
                "SELECT content_hash, first_seen_at, last_seen_at
                   FROM chunk_gc_candidates
                  WHERE first_seen_at < $1
                  ORDER BY first_seen_at
                  LIMIT $2",
            )
            .bind(cutoff)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?
        } else {
            sqlx::query_as::<
                _,
                (
                    Vec<u8>,
                    chrono::DateTime<chrono::Utc>,
                    chrono::DateTime<chrono::Utc>,
                ),
            >(
                "SELECT content_hash, first_seen_at, last_seen_at
                   FROM chunk_gc_candidates
                  ORDER BY first_seen_at
                  LIMIT $1",
            )
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?
        };
        let mut out = Vec::with_capacity(rows.len());
        for (bytes, first_seen_at, last_seen_at) in rows {
            if bytes.len() != 32 {
                return Err(MetaError::Serialization(format!(
                    "chunk_gc_candidates.content_hash has {} bytes, expected 32",
                    bytes.len()
                )));
            }
            let mut content_hash = [0u8; 32];
            content_hash.copy_from_slice(&bytes);
            out.push(engram_core::traits::GcCandidateRow {
                content_hash,
                first_seen_at,
                last_seen_at,
            });
        }
        Ok(out)
    }

    /// ADR 0016 Phase C: batch-delete candidate rows. Empty input
    /// is a no-op (skip the round-trip). PG handles the array via
    /// `ANY($1::bytea[])`.
    async fn delete_gc_candidates(&self, hashes: &[[u8; 32]]) -> Result<(), MetaError> {
        if hashes.is_empty() {
            return Ok(());
        }
        let as_vecs: Vec<&[u8]> = hashes.iter().map(|h| h.as_slice()).collect();
        sqlx::query("DELETE FROM chunk_gc_candidates WHERE content_hash = ANY($1::bytea[])")
            .bind(&as_vecs)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }
}
