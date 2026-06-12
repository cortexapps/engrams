//! Postgres-backed [`MetadataStore`] implementation.
//!
//! Queries are written against the schema in `deploy/migrations/0001_initial.sql`.
//! sqlx's compile-time checking is intentionally disabled here — runtime
//! queries let the crate build without a live database, which keeps the
//! workspace usable for early-stage development. We can flip to
//! `query!`/`query_as!` once CI provisions a Postgres service and we
//! ship a `.sqlx/` cache.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use engram_core::traits::{DisableEnabledImageOutcome, MetadataStore};
use engram_core::types::{
    ArtifactRow, EnableJob, EnableJobState, EnabledImage, HostCapacity, HostRecord, HostStatus,
    HostUtilization, PersistedEvent, RegistryCredential, Session, SessionSecrets, SessionSpec,
    SessionState, SnapshotRecord,
};
use engram_core::{HostId, MetaError, SandboxId, SessionId};
use row::col_err;
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
        // Retry the initial connect with backoff. In prod PG (Cloud SQL)
        // is always up, so the first attempt succeeds with no delay. But
        // a freshly-started PG — the docker-compose container in CI's
        // `test-e2e-stack` lane, or a just-provisioned instance — can
        // reset the first connections while it finishes booting. Without
        // a retry the coord exits at startup ("postgres connect:
        // Connection reset by peer") and the e2e-stack bring-up flakes
        // ("coord never came up"). ~10 attempts over ~20s covers the
        // container-startup race without masking a genuinely-bad URL.
        const MAX_ATTEMPTS: u32 = 10;
        let mut attempt = 0;
        let pool = loop {
            attempt += 1;
            match PgPoolOptions::new().max_connections(8).connect(url).await {
                Ok(pool) => break pool,
                Err(e) if attempt < MAX_ATTEMPTS => {
                    tracing::warn!(
                        attempt,
                        max_attempts = MAX_ATTEMPTS,
                        error = %e,
                        "postgres connect failed; retrying after backoff",
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(
                        500 * u64::from(attempt.min(6)),
                    ))
                    .await;
                }
                Err(e) => return Err(MetaError::Db(Box::new(e))),
            }
        };
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

/// ADR 0046: least-loaded-that-fits among `candidates` (ranked). `free = total
/// − reserved − residency_floor`; ties break toward the earlier (affinity-
/// preferred) candidate. `None` → no candidate has room → caller rejects (503).
/// Pure (no I/O) so it's unit-tested without a database — it is the placement
/// decision the OOM incident needed.
fn choose_placement_host(
    candidates: &[uuid::Uuid],
    allocatable_mib: &std::collections::HashMap<uuid::Uuid, i64>,
    reserved_mib: &std::collections::HashMap<uuid::Uuid, i64>,
    budget_mib: i64,
) -> Option<uuid::Uuid> {
    let mut best_known: Option<(i64, uuid::Uuid)> = None;
    let mut fallback_unknown: Option<uuid::Uuid> = None;
    for h in candidates {
        let Some(&alloc) = allocatable_mib.get(h) else {
            continue; // not ready/draining at lock time → skip
        };
        if alloc <= 0 {
            // No allocatable measurement yet (non-Linux dev backend, pre-0058,
            // or a brand-new host) — don't gate on a bogus 0; keep as a
            // last-resort fallback so dev/new hosts still take work.
            fallback_unknown.get_or_insert(*h);
            continue;
        }
        let free = alloc - reserved_mib.get(h).copied().unwrap_or(0);
        if free < budget_mib {
            continue;
        }
        match best_known {
            Some((bf, _)) if bf >= free => {}
            _ => best_known = Some((free, *h)),
        }
    }
    best_known.map(|(_, h)| h).or(fallback_unknown)
}

// Kept beside `choose_placement_host` (the fn it exercises) rather than at the
// file end — the `MetadataStore` impl follows.
#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod placement_tests {
    use super::choose_placement_host;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn ids(n: usize) -> Vec<Uuid> {
        (1..=n as u128).map(Uuid::from_u128).collect()
    }

    #[test]
    fn picks_least_loaded_that_fits() {
        let h = ids(2);
        let total: HashMap<_, _> = [(h[0], 32768i64), (h[1], 32768)].into();
        let reserved: HashMap<_, _> = [(h[0], 28000i64)].into(); // h0 nearly full
        assert_eq!(
            choose_placement_host(&h, &total, &reserved, 4096),
            Some(h[1])
        );
    }

    #[test]
    fn rejects_when_none_fit() {
        let h = ids(2);
        let total: HashMap<_, _> = [(h[0], 8192i64), (h[1], 8192)].into();
        let reserved: HashMap<_, _> = [(h[0], 6000i64), (h[1], 6000)].into();
        assert_eq!(choose_placement_host(&h, &total, &reserved, 4096), None);
    }

    #[test]
    fn unknown_allocatable_is_a_fallback_not_a_gate() {
        let h = ids(2);
        // h0 unmeasured (0), h1 measured + fits → prefer the measured host.
        let alloc: HashMap<_, _> = [(h[0], 0i64), (h[1], 32768)].into();
        assert_eq!(
            choose_placement_host(&h, &alloc, &HashMap::new(), 4096),
            Some(h[1])
        );
        // only the unmeasured host (dev backend / brand-new) → fall back to it.
        let only0: HashMap<_, _> = [(h[0], 0i64)].into();
        assert_eq!(
            choose_placement_host(&h[..1], &only0, &HashMap::new(), 4096),
            Some(h[0])
        );
    }

    #[test]
    fn tie_breaks_toward_earlier_candidate() {
        let h = ids(2);
        let total: HashMap<_, _> = [(h[0], 32768i64), (h[1], 32768)].into();
        assert_eq!(
            choose_placement_host(&h, &total, &HashMap::new(), 4096),
            Some(h[0])
        );
    }

    /// The incident in miniature: a burst of 4 GiB sessions onto two ~16 GiB
    /// hosts. With each reservation feeding the next pick (as the FOR UPDATE
    /// txn makes real), placement spreads evenly and rejects the overflow
    /// instead of stacking onto one host and OOM-ing it.
    #[test]
    fn burst_spreads_then_rejects_overflow() {
        let h = ids(2);
        let total: HashMap<_, _> = [(h[0], 16384i64), (h[1], 16384)].into();
        let mut reserved: HashMap<Uuid, i64> = HashMap::new();
        let budget = 4096;
        let mut picks = Vec::new();
        for _ in 0..10 {
            match choose_placement_host(&h, &total, &reserved, budget) {
                Some(p) => {
                    *reserved.entry(p).or_default() += budget;
                    picks.push(Some(p));
                }
                None => picks.push(None),
            }
        }
        let placed = picks.iter().filter(|p| p.is_some()).count();
        assert_eq!(placed, 8, "16384/4096 = 4 per host = 8 total fit");
        let on0 = picks.iter().flatten().filter(|&&p| p == h[0]).count();
        let on1 = picks.iter().flatten().filter(|&&p| p == h[1]).count();
        assert_eq!((on0, on1), (4, 4), "spread evenly, not stacked");
    }
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
        // ADR 0021 P1.3: `mode` is a flat text column now (migration
        // 0039); `SessionMode::as_str` renders the CHECK-valid value.
        let mode_text = spec.mode.as_str();
        sqlx::query(
            r#"
            INSERT INTO sessions
                (id, status, host_id,
                 image_uri, mode,
                 created_at, last_active_at)
            VALUES ($1, $2, NULL, $3, $4, $5, $5)
            "#,
        )
        .bind(id)
        .bind(SessionState::Pending.as_str())
        .bind(&spec.image)
        .bind(mode_text)
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
        // ADR 0021 P1.3: `mode` is a flat text column now (migration
        // 0039); `SessionMode::as_str` renders the CHECK-valid value.
        let mode_text = spec.mode.as_str();
        sqlx::query(
            r#"
            INSERT INTO sessions
                (id, status, host_id, sandbox_id,
                 image_uri, mode,
                 created_at, last_active_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $7)
            ON CONFLICT (id) DO UPDATE SET
                status         = EXCLUDED.status,
                sandbox_id     = EXCLUDED.sandbox_id,
                host_id        = EXCLUDED.host_id,
                last_active_at = EXCLUDED.last_active_at
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(SessionState::Created.as_str())
        .bind(host_id.as_uuid())
        .bind(sandbox_id.as_uuid())
        .bind(&spec.image)
        .bind(mode_text)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn reserve_placement(
        &self,
        session_id: SessionId,
        spec: &SessionSpec,
        mem_budget_mib: i64,
        candidates: &[HostId],
    ) -> Result<Option<HostId>, MetaError> {
        if candidates.is_empty() {
            return Ok(None);
        }
        let cand: Vec<uuid::Uuid> = candidates.iter().map(|h| h.as_uuid()).collect();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        // Lock the candidate host rows so concurrent placers (any coord replica)
        // serialize on the overlap — a burst can't read the same pre-insert
        // reserved figure and stack onto one host. Held only for the pick +
        // insert below (sub-ms).
        let host_rows = sqlx::query(
            r#"
            SELECT id, allocatable_mib
            FROM hosts
            WHERE id = ANY($1) AND status IN ('ready','draining')
            FOR UPDATE
            "#,
        )
        .bind(&cand)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        // ADR 0046: allocatable_mib is the host-measured headroom for new
        // sessions (MemAvailable + Σ guest-resident) — it already nets out the
        // daemon / OS / chunk-cache / mlock'd residency baseline, so we subtract
        // only session budgets from it. 0 = no measurement yet (dev/pre-0058).
        let mut alloc: std::collections::HashMap<uuid::Uuid, i64> =
            std::collections::HashMap::with_capacity(host_rows.len());
        for r in &host_rows {
            let id: uuid::Uuid = sqlx::Row::try_get(r, "id").map_err(db_err)?;
            let a: i64 = sqlx::Row::try_get(r, "allocatable_mib").map_err(db_err)?;
            alloc.insert(id, a);
        }
        // Reserved within the txn — sees the committed `pending` rows of placers
        // that locked these hosts before us. Status list is the SQL twin of
        // `SessionState::host_memory_reserving_states()`.
        let res_rows = sqlx::query(
            r#"
            SELECT host_id, COALESCE(SUM(mem_budget_mib), 0)::BIGINT AS reserved_mib
            FROM sessions
            WHERE host_id = ANY($1)
              AND status IN ('pending','created','guest_ready','active',
                             'evacuating','evicting')
              -- A `pending` row older than 10 min is a crash-orphaned
              -- reservation (a boot never takes that long); don't let it leak
              -- into the reserved figure and false-reject the host.
              AND (status <> 'pending' OR created_at > NOW() - INTERVAL '10 minutes')
            GROUP BY host_id
            "#,
        )
        .bind(&cand)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        let mut reserved: std::collections::HashMap<uuid::Uuid, i64> =
            std::collections::HashMap::with_capacity(res_rows.len());
        for r in &res_rows {
            let h: uuid::Uuid = sqlx::Row::try_get(r, "host_id").map_err(db_err)?;
            let v: i64 = sqlx::Row::try_get(r, "reserved_mib").map_err(db_err)?;
            reserved.insert(h, v);
        }
        // Least-loaded-that-fits among the (ranked) candidates — see
        // `choose_placement_host` (unit-tested).
        let Some(picked) = choose_placement_host(&cand, &alloc, &reserved, mem_budget_mib) else {
            tx.rollback().await.map_err(db_err)?;
            return Ok(None);
        };
        let now = Utc::now();
        sqlx::query(
            r#"
            INSERT INTO sessions
                (id, status, host_id, sandbox_id,
                 image_uri, mode, mem_budget_mib, created_at, last_active_at)
            VALUES ($1, 'pending', $2, NULL, $3, $4, $5, $6, $6)
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(picked)
        .bind(&spec.image)
        .bind(spec.mode.as_str())
        .bind(mem_budget_mib)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(Some(HostId(picked)))
    }

    async fn delete_pending_session(&self, session_id: SessionId) -> Result<(), MetaError> {
        sqlx::query(
            "DELETE FROM sessions WHERE id = $1 AND status = 'pending' AND sandbox_id IS NULL",
        )
        .bind(session_id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_session(&self, id: SessionId) -> Result<Session, MetaError> {
        let row = sqlx::query(
            r#"
            SELECT id, status, host_id, sandbox_id,
                   image_uri, mode,
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

    /// ADR 0046: Σ over schedulable hosts of `max(0, allocatable − reserved)` —
    /// the real fleet free-memory signal (replaces the phantom `total − used`).
    /// `pending` reservations older than 10 min are excluded as crash-orphaned
    /// (a boot never takes that long), matching `reserve_placement`.
    async fn fleet_free_mib(&self) -> Result<i64, MetaError> {
        let free: i64 = sqlx::query_scalar(
            r#"
            SELECT COALESCE(SUM(GREATEST(0, h.allocatable_mib - COALESCE(r.reserved, 0))), 0)::BIGINT
            FROM hosts h
            LEFT JOIN (
                SELECT host_id, SUM(mem_budget_mib) AS reserved
                FROM sessions
                WHERE host_id IS NOT NULL
                  AND status IN ('pending','created','guest_ready','active',
                                 'evacuating','evicting')
                  AND (status <> 'pending' OR created_at > NOW() - INTERVAL '10 minutes')
                GROUP BY host_id
            ) r ON r.host_id = h.id
            WHERE h.status IN ('ready','draining')
            "#,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(free)
    }

    async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError> {
        // Every non-terminal state except `host_lost` (limbo pending the
        // reconciler; its bindings are stale by definition).
        // `evicting` matters most: it keeps `sandbox_id` BOUND
        // while the pipeline runs, and startup's `repopulate_routing`
        // rebuilds the in-memory SandboxRegistry from this query — when
        // `evicting` was missing, a coord roll mid-eviction left the new
        // pod's registry empty for that session, every scanner attempt
        // no-op'd on the "sandbox no longer bound" guard, and the budget
        // exhausted into a spurious HostLost with the VM still running
        // (prod session 5cfb90b8, 2026-06-03).
        let rows = sqlx::query(
            r#"
            SELECT id, status, host_id, sandbox_id,
                   image_uri, mode,
                   created_at, last_active_at,
                   live_disk_manifest_id, live_disk_manifest_version
            FROM sessions
            WHERE status IN ('pending','created','guest_ready','active',
                             'idle','evacuating','evicting')
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
        // clean. ADR 0034 mirrors this for Evicting/`evict_attempts`.
        // Folded into the same UPDATE that commits the state flip so
        // the counters and the state are always consistent.
        sqlx::query(
            r#"
            UPDATE sessions
               SET status = $2,
                   last_active_at = NOW(),
                   updated_at = NOW(),
                   evac_attempts = CASE WHEN $2 = 'evacuating' THEN 0 ELSE evac_attempts END,
                   evict_attempts = CASE WHEN $2 = 'evicting' THEN 0 ELSE evict_attempts END
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
            SELECT id, status, host_id, sandbox_id,
                   image_uri, mode,
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

    /// ADR 0034: eviction-scanner sweep query. Indexed via the
    /// partial `idx_sessions_evicting` from migration 0050 so the
    /// cost stays flat as the global session row count grows.
    async fn list_evicting_sessions(&self) -> Result<Vec<(Session, u32)>, MetaError> {
        let rows = sqlx::query(
            r#"
            SELECT id, status, host_id, sandbox_id,
                   image_uri, mode,
                   created_at, last_active_at,
                   live_disk_manifest_id, live_disk_manifest_version,
                   evict_attempts
            FROM sessions
            WHERE status = 'evicting'
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let session = row::session_from_row(r)?;
            let attempts: i32 = r
                .try_get("evict_attempts")
                .map_err(|e| MetaError::Serialization(format!("evict_attempts: {e}")))?;
            out.push((session, attempts.max(0) as u32));
        }
        Ok(out)
    }

    /// ADR 0034: atomic `+= 1 RETURNING`. The eviction scanner calls
    /// this before each pipeline attempt; when the returned count
    /// crosses the budget it falls back to HostLost.
    async fn bump_evict_attempts(&self, session_id: SessionId) -> Result<u32, MetaError> {
        let row = sqlx::query(
            r#"
            UPDATE sessions
               SET evict_attempts = evict_attempts + 1
             WHERE id = $1
             RETURNING evict_attempts
            "#,
        )
        .bind(session_id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or(MetaError::NotFound)?;
        let attempts: i32 = row
            .try_get("evict_attempts")
            .map_err(|e| MetaError::Serialization(format!("bump_evict_attempts: {e}")))?;
        Ok(attempts.max(0) as u32)
    }

    /// ADR 0034 L3 backstop: Active sessions whose newest
    /// session_events row is older than `idle_for_secs`. COALESCE to
    /// the session's own `created_at` covers a freshly-created Active
    /// session that has not emitted events yet (it still gets the
    /// full TTL before the backstop will touch it). The per-session
    /// MAX is served by `idx_session_events_session_created`
    /// (migration 0050).
    async fn list_active_sessions_idle_past(
        &self,
        idle_for_secs: i64,
    ) -> Result<Vec<(SessionId, SandboxId, chrono::DateTime<chrono::Utc>)>, MetaError> {
        let rows = sqlx::query(
            r#"
            SELECT s.id, s.sandbox_id,
                   COALESCE(MAX(e.created_at), s.created_at) AS last_event_at
            FROM sessions s
            LEFT JOIN session_events e ON e.session_id = s.id
            WHERE s.status = 'active' AND s.sandbox_id IS NOT NULL
            GROUP BY s.id, s.sandbox_id, s.created_at
            HAVING COALESCE(MAX(e.created_at), s.created_at)
                   < NOW() - ($1::bigint * INTERVAL '1 second')
            "#,
        )
        .bind(idle_for_secs)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let id: uuid::Uuid = r
                .try_get("id")
                .map_err(|e| MetaError::Serialization(format!("backstop id: {e}")))?;
            let sandbox: uuid::Uuid = r
                .try_get("sandbox_id")
                .map_err(|e| MetaError::Serialization(format!("backstop sandbox_id: {e}")))?;
            let last_event_at: chrono::DateTime<chrono::Utc> = r
                .try_get("last_event_at")
                .map_err(|e| MetaError::Serialization(format!("backstop last_event_at: {e}")))?;
            out.push((SessionId::from(id), SandboxId::from(sandbox), last_event_at));
        }
        Ok(out)
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
            ON CONFLICT (id) DO UPDATE SET
                hostname                = EXCLUDED.hostname,
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
                   util_disk_total_mib, util_disk_used_mib,
                   util_mem_total_mib, util_mem_used_mib, util_cpu_pct,
                   allocatable_mib,
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
        utilization: HostUtilization,
    ) -> Result<(), MetaError> {
        let n = sqlx::query(
            r#"UPDATE hosts
                  SET status = $2,
                      capacity_total_mib = $3,
                      capacity_used_mib = $4,
                      running_sandboxes_count = $5,
                      util_disk_total_mib = $6,
                      util_disk_used_mib = $7,
                      util_mem_total_mib = $8,
                      util_mem_used_mib = $9,
                      util_cpu_pct = $10,
                      allocatable_mib = $11,
                      last_heartbeat_at = NOW(),
                      updated_at = NOW()
                WHERE id = $1"#,
        )
        .bind(id.as_uuid())
        .bind(status.as_str())
        .bind(capacity.total_mib as i64)
        .bind(capacity.used_mib as i64)
        .bind(capacity.running_sandboxes as i32)
        .bind(utilization.disk_total_mib as i64)
        .bind(utilization.disk_used_mib as i64)
        .bind(utilization.mem_total_mib as i64)
        .bind(utilization.mem_used_mib as i64)
        .bind(utilization.cpu_pct)
        .bind(utilization.allocatable_mib as i64)
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
                   util_disk_total_mib, util_disk_used_mib,
                   util_mem_total_mib, util_mem_used_mib, util_cpu_pct,
                   allocatable_mib,
                   last_heartbeat_at, status, host_addr
              FROM hosts
             -- Only `ready` hosts are strike-out candidates. A `draining`
             -- host is operator-managed: mid image-roll (where ADR 0044 K2
             -- reattach keeps its VMs alive across the brief pod-swap
             -- heartbeat gap) or mid node-removal (where its sessions are
             -- already being evacuated). The operator owns its lifecycle, so
             -- the dead-host detector must not race a roll and route the
             -- reattaching sessions to Idle out from under the successor.
             -- (ADR 0044 K3: image rolls reattach, not evacuate.)
             WHERE status = 'ready'
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
                 recoverable, aux_bundles, events_cursor)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
            ON CONFLICT (id) DO UPDATE SET
                last_accessed_at        = EXCLUDED.last_accessed_at,
                disk_manifest_id        = EXCLUDED.disk_manifest_id,
                disk_manifest_version   = EXCLUDED.disk_manifest_version,
                memory_manifest_id      = EXCLUDED.memory_manifest_id,
                memory_manifest_version = EXCLUDED.memory_manifest_version,
                recoverable             = EXCLUDED.recoverable,
                aux_bundles             = EXCLUDED.aux_bundles,
                -- ADR 0028 A.log: never clobber a resolved cursor with
                -- NULL on an idempotent re-record (the reconciler may
                -- re-ingest a checkpoint the eviction pipeline already
                -- recorded with a cursor, or vice versa).
                events_cursor           = COALESCE(EXCLUDED.events_cursor, snapshots.events_cursor),
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
        .bind(
            serde_json::to_value(&snap.aux_bundles)
                .map_err(|e| MetaError::Serialization(format!("aux_bundles encode: {e}")))?,
        )
        .bind(snap.events_cursor)
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

    async fn prune_session_snapshots(
        &self,
        retention: chrono::Duration,
    ) -> Result<Vec<engram_core::types::SnapshotId>, MetaError> {
        // ADR 0028 Fix A retention: keep each session's latest row
        // unconditionally (DISTINCT ON newest-first); delete the rest
        // past the window. Same-TX chunk_generation bump keeps the GC
        // barrier semantics symmetric with record_snapshot — a sweep
        // that read the pin set mid-prune restarts and re-classifies.
        // RETURNING the deleted ids lets the caller delete each row's
        // portable `snapshots/<id>/` blobs (state.bin / sidecar), which
        // the chunk GC doesn't sweep (different namespace) and would
        // otherwise orphan in BlobStorage.
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let rows: Vec<(uuid::Uuid,)> = sqlx::query_as(
            r#"
            DELETE FROM snapshots s
            WHERE s.session_id IS NOT NULL
              AND s.created_at < NOW() - $1::interval
              AND s.id NOT IN (
                  SELECT DISTINCT ON (session_id) id
                  FROM snapshots
                  WHERE session_id IS NOT NULL
                  ORDER BY session_id, created_at DESC
              )
            RETURNING s.id
            "#,
        )
        .bind(sqlx::postgres::types::PgInterval {
            months: 0,
            days: 0,
            microseconds: retention.num_microseconds().unwrap_or(i64::MAX),
        })
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query("UPDATE chunk_generation SET generation = generation + 1 WHERE id = TRUE")
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|(id,)| engram_core::types::SnapshotId::from(id))
            .collect())
    }

    async fn latest_event_idx_at_or_before(
        &self,
        sid: SessionId,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<i64>, MetaError> {
        // ADR 0028 A.log: the checkpoint pause instant freezes the
        // guest, so events it caused can't land in the captured state
        // after `at`. (Events emitted just before pause but written
        // just after are a sub-second edge we accept — they survive a
        // rewind the agent technically remembers.)
        let row: (Option<i64>,) = sqlx::query_as(
            "SELECT MAX(idx) FROM session_events WHERE session_id = $1 AND created_at <= $2",
        )
        .bind(sid.as_uuid())
        .bind(at)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(row.0)
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
                   recoverable, aux_bundles, events_cursor
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
                   recoverable, aux_bundles, events_cursor
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
                   recoverable, aux_bundles, events_cursor
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
        // ADR 0028 A.log: stamp the session's current `recovery_epoch`
        // on the new event (read in the same CTE as the idx bump, so
        // it reflects any rewind that already committed). Pre-rewind
        // sessions stay epoch 0.
        let row = sqlx::query(
            r#"
            WITH next AS (
                UPDATE sessions
                   SET next_event_idx = next_event_idx + 1,
                       updated_at = NOW()
                 WHERE id = $1
             RETURNING next_event_idx - 1 AS allocated_idx, recovery_epoch
            ),
            inserted AS (
                INSERT INTO session_events (session_id, idx, kind, payload, recovery_epoch)
                SELECT $1, allocated_idx, $2, $3, recovery_epoch FROM next
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
        // ADR 0028 A.log: replay ALL events (incl. tombstoned), each
        // carrying its `recovery_epoch` + `rewound_at`. The transcript
        // renders rewound rows collapsed/greyed and segments by epoch —
        // honest history, not a silent deletion.
        let rows = sqlx::query(
            r#"
            SELECT idx, kind, payload, created_at, recovery_epoch, rewound_at
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

    async fn rewind_session_to_cursor(
        &self,
        session_id: SessionId,
        events_cursor: i64,
    ) -> Result<engram_core::types::event::RewindSummary, MetaError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        // Surviving side-effects: outside-world actions in the
        // rolled-back span that the rewind CANNOT undo. We surface
        // them rather than hide them (the deliberate at-least-once
        // posture). Detect the kinds that touched the world.
        let side_effect_rows = sqlx::query(
            r#"
            SELECT kind, payload FROM session_events
             WHERE session_id = $1 AND idx > $2 AND rewound_at IS NULL
               AND kind IN ('pull_request_opened', 'file_shared')
             ORDER BY idx
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(events_cursor)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        let surviving_side_effects = side_effect_rows
            .iter()
            .filter_map(|r| {
                let kind: String = sqlx::Row::try_get(r, "kind").ok()?;
                let payload: serde_json::Value = sqlx::Row::try_get(r, "payload").ok()?;
                Some(match kind.as_str() {
                    "pull_request_opened" => format!(
                        "A pull request was opened and still exists: {}",
                        payload
                            .get("url")
                            .and_then(|v| v.as_str())
                            .unwrap_or("(url unknown)"),
                    ),
                    "file_shared" => format!(
                        "A file was shared and still exists: {}",
                        payload
                            .get("caption")
                            .and_then(|v| v.as_str())
                            .or_else(|| payload.get("artifact_id").and_then(|v| v.as_str()))
                            .unwrap_or("(artifact)"),
                    ),
                    _ => return None,
                })
            })
            .collect();

        // Tombstone the rolled-back span (audit-preserving) and count it.
        let tombstoned = sqlx::query(
            r#"
            UPDATE session_events
               SET rewound_at = NOW()
             WHERE session_id = $1 AND idx > $2 AND rewound_at IS NULL
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(events_cursor)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();

        if tombstoned == 0 {
            // Checkpoint was already the head — no rewind. Don't bump
            // the epoch (keeps the no-op clean); caller emits nothing.
            tx.rollback().await.map_err(db_err)?;
            return Ok(engram_core::types::event::RewindSummary::default());
        }

        // Bump the epoch so events appended after this segment cleanly.
        let epoch_row = sqlx::query(
            "UPDATE sessions SET recovery_epoch = recovery_epoch + 1, updated_at = NOW() \
             WHERE id = $1 RETURNING recovery_epoch",
        )
        .bind(session_id.as_uuid())
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let recovery_epoch: i32 =
            sqlx::Row::try_get(&epoch_row, "recovery_epoch").map_err(db_err)?;

        tx.commit().await.map_err(db_err)?;
        Ok(engram_core::types::event::RewindSummary {
            rolled_back: tombstoned,
            recovery_epoch: recovery_epoch as i64,
            through_idx: events_cursor,
            surviving_side_effects,
        })
    }

    // ---------- file artifacts (ADR 0026) ----------

    async fn insert_artifact(
        &self,
        id: uuid::Uuid,
        session_id: SessionId,
        blob_key: &str,
        media_type: &str,
        size_bytes: i64,
        caption: Option<&str>,
    ) -> Result<(), MetaError> {
        sqlx::query(
            r#"
            INSERT INTO artifacts (id, session_id, blob_key, media_type, size_bytes, caption)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
        )
        .bind(id)
        .bind(session_id.as_uuid())
        .bind(blob_key)
        .bind(media_type)
        .bind(size_bytes)
        .bind(caption)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_artifact(
        &self,
        session_id: SessionId,
        id: uuid::Uuid,
    ) -> Result<Option<ArtifactRow>, MetaError> {
        // Scoped to the session so one session can never read another's
        // blob key even with a guessed artifact id.
        let row = sqlx::query(
            r#"
            SELECT id, blob_key, media_type, size_bytes, caption, created_at
              FROM artifacts
             WHERE id = $1 AND session_id = $2
            "#,
        )
        .bind(id)
        .bind(session_id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.as_ref().map(row::artifact_from_row).transpose()
    }

    async fn artifact_usage(&self, session_id: SessionId) -> Result<(i64, i64), MetaError> {
        let row = sqlx::query(
            r#"
            SELECT COUNT(*)::bigint AS n,
                   COALESCE(SUM(size_bytes), 0)::bigint AS total
              FROM artifacts
             WHERE session_id = $1
            "#,
        )
        .bind(session_id.as_uuid())
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        let n: i64 = sqlx::Row::try_get(&row, "n").map_err(db_err)?;
        let total: i64 = sqlx::Row::try_get(&row, "total").map_err(db_err)?;
        Ok((n, total))
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

    // ADR 0021 P1.5a retired the harness-packs CRUD here; the trait
    // surface no longer carries them and migration 0040 drops the
    // backing Postgres table.

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
                 base_snapshot_disk_manifest_id, base_snapshot_disk_manifest_version,
                 base_snapshot_memory_manifest_id, base_snapshot_memory_manifest_version,
                 last_refreshed_at, created_at, updated_at, soft_deleted_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, NULL, NULL)
            ON CONFLICT (image_uri) DO UPDATE SET
                manifest_toml         = EXCLUDED.manifest_toml,
                manifest_digest       = EXCLUDED.manifest_digest,
                disk_manifest_id      = EXCLUDED.disk_manifest_id,
                disk_manifest_version = EXCLUDED.disk_manifest_version,
                base_snapshot_id      = EXCLUDED.base_snapshot_id,
                base_snapshot_disk_manifest_id      = EXCLUDED.base_snapshot_disk_manifest_id,
                base_snapshot_disk_manifest_version = EXCLUDED.base_snapshot_disk_manifest_version,
                base_snapshot_memory_manifest_id      = EXCLUDED.base_snapshot_memory_manifest_id,
                base_snapshot_memory_manifest_version = EXCLUDED.base_snapshot_memory_manifest_version,
                last_refreshed_at     = EXCLUDED.last_refreshed_at,
                updated_at            = NOW(),
                -- ADR 0021 P1.8: enabling an image always "undeletes" any
                -- prior soft-delete on the same image_uri. Operator who
                -- disabled v1.0.0 then re-enables it gets a live row again,
                -- without needing a separate undelete flow. The row's
                -- chunks were never GC'd (chunk-GC's pin-set counts
                -- soft-deleted images as still-referenced), so the
                -- transition is a pure metadata flip.
                soft_deleted_at       = NULL
            "#,
        )
        .bind(image.id)
        .bind(&image.image_uri)
        .bind(&image.manifest_toml)
        .bind(&image.manifest_digest)
        .bind(image.disk_manifest.map(|m| m.manifest_id))
        .bind(image.disk_manifest.map(|m| m.version as i64))
        .bind(image.base_snapshot_id.map(|s| s.as_uuid()))
        .bind(image.base_snapshot_disk_manifest.map(|m| m.manifest_id))
        .bind(image.base_snapshot_disk_manifest.map(|m| m.version as i64))
        .bind(image.base_snapshot_memory_manifest.map(|m| m.manifest_id))
        .bind(image.base_snapshot_memory_manifest.map(|m| m.version as i64))
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
        // ADR 0021 P1.8: live-only filter. Hosts advertise + the
        // dashboard surfaces only `soft_deleted_at IS NULL`. The
        // resume path's lookup uses `get_enabled_image_any` instead.
        let rows = sqlx::query(
            r#"
            SELECT id, image_uri, manifest_toml, manifest_digest,
                   disk_manifest_id, disk_manifest_version, base_snapshot_id,
                   base_snapshot_disk_manifest_id, base_snapshot_disk_manifest_version,
                   base_snapshot_memory_manifest_id, base_snapshot_memory_manifest_version,
                   last_refreshed_at, created_at, updated_at, soft_deleted_at
              FROM enabled_images
             WHERE soft_deleted_at IS NULL
             ORDER BY image_uri
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row::enabled_image_from_row).collect()
    }

    async fn get_enabled_image(&self, image_uri: &str) -> Result<Option<EnabledImage>, MetaError> {
        // ADR 0021 P1.8: live-only filter. Callers on the
        // session-create path get None for a disabled image and
        // surface "not enabled" to the user. The resume path uses
        // `get_enabled_image_any` to look past the flag.
        let row = sqlx::query(
            r#"
            SELECT id, image_uri, manifest_toml, manifest_digest,
                   disk_manifest_id, disk_manifest_version, base_snapshot_id,
                   base_snapshot_disk_manifest_id, base_snapshot_disk_manifest_version,
                   base_snapshot_memory_manifest_id, base_snapshot_memory_manifest_version,
                   last_refreshed_at, created_at, updated_at, soft_deleted_at
              FROM enabled_images
             WHERE image_uri = $1 AND soft_deleted_at IS NULL
            "#,
        )
        .bind(image_uri)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| row::enabled_image_from_row(&r)).transpose()
    }

    async fn get_enabled_image_any(
        &self,
        image_uri: &str,
    ) -> Result<Option<EnabledImage>, MetaError> {
        // ADR 0021 P1.8: resume-path lookup. Returns the row even if
        // soft-deleted, so a session whose image was disabled while
        // it was idle can still resume. Caller is expected to be on
        // a resume codepath; the session-create path uses
        // `get_enabled_image` (live-filtered).
        let row = sqlx::query(
            r#"
            SELECT id, image_uri, manifest_toml, manifest_digest,
                   disk_manifest_id, disk_manifest_version, base_snapshot_id,
                   base_snapshot_disk_manifest_id, base_snapshot_disk_manifest_version,
                   base_snapshot_memory_manifest_id, base_snapshot_memory_manifest_version,
                   last_refreshed_at, created_at, updated_at, soft_deleted_at
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

    async fn soft_delete_enabled_image(
        &self,
        image_uri: &str,
    ) -> Result<DisableEnabledImageOutcome, MetaError> {
        // ADR 0021 P1.8: guarded soft-delete.
        //
        // Inside one transaction: take a row-level lock on the
        // enabled_images row (FOR UPDATE), count sessions in
        // states that pin the image, return the blocking list if
        // nonzero, else flip `soft_deleted_at = NOW()`. The row-
        // level lock + session counting in the same TX closes the
        // race where a session-create starts between the count and
        // the UPDATE — the create's own `get_enabled_image` lookup
        // takes the same lock and serialises.
        //
        // Blocking states: {pending, created, active, evacuating}.
        // Sessions in {idle, completed, dead, failed} don't block
        // because they either can resume against the soft-deleted
        // row or are terminal.
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        // Step 1: lock the image row. Returns NotFound if no row;
        // returns Ok with no-op semantics if already soft-deleted
        // (idempotent — second `disable` of the same URI is a no-op).
        let row = sqlx::query(
            "SELECT id, soft_deleted_at FROM enabled_images \
             WHERE image_uri = $1 \
             FOR UPDATE",
        )
        .bind(image_uri)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        let Some(row) = row else {
            return Err(MetaError::NotFound);
        };
        let already_soft_deleted: Option<DateTime<Utc>> =
            row.try_get("soft_deleted_at").map_err(col_err)?;
        if already_soft_deleted.is_some() {
            // Idempotent: tell the caller it was already disabled,
            // no further work to do. Tx commits with no changes.
            tx.commit().await.map_err(db_err)?;
            return Ok(DisableEnabledImageOutcome::AlreadyDisabled);
        }

        // Step 2: blocking-session check.
        let blocking: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT id, status FROM sessions \
             WHERE image_uri = $1 \
               AND status IN ('pending', 'created', 'active', 'evacuating') \
             ORDER BY created_at ASC \
             LIMIT 16",
        )
        .bind(image_uri)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        if !blocking.is_empty() {
            // Don't commit — leave the row untouched. The lock
            // releases on tx drop.
            return Ok(DisableEnabledImageOutcome::Blocked(
                blocking
                    .into_iter()
                    .map(|(id, status)| (SessionId(id), status))
                    .collect(),
            ));
        }

        // Step 3: flip the bit + bump chunk_generation so any future
        // chunk-GC sweep that read the pin-set pre-flip observes
        // divergence and restarts. (Soft-deleted images are still in
        // the pin-set today; the bump is forward-compat with a
        // future GC that treats them as candidates.)
        sqlx::query(
            "UPDATE enabled_images \
             SET soft_deleted_at = NOW(), updated_at = NOW() \
             WHERE image_uri = $1",
        )
        .bind(image_uri)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query("UPDATE chunk_generation SET generation = generation + 1 WHERE id = TRUE")
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(DisableEnabledImageOutcome::Disabled)
    }

    async fn delete_enabled_image(&self, image_uri: &str) -> Result<(), MetaError> {
        // ADR 0021 P1.8: physical DELETE is reserved for chunk-GC.
        // Routine "disable" goes through `soft_delete_enabled_image`.
        // This trait method stays for forward use by the future GC
        // sweeper once a row's chunks are confirmed unreferenced.
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

    async fn find_enabled_image_by_content(
        &self,
        disk_manifest: engram_core::types::manifest::ManifestRef,
        manifest_toml: &str,
    ) -> Result<Option<EnabledImage>, MetaError> {
        // Soft-deleted rows are deliberately INCLUDED: their base
        // snapshots remain GC-pinned and restorable, and content
        // equality is what makes the reuse sound — liveness of the
        // *row* is irrelevant to the snapshot's validity.
        let row = sqlx::query(
            r#"
            SELECT id, image_uri, manifest_toml, manifest_digest,
                   disk_manifest_id, disk_manifest_version, base_snapshot_id,
                   base_snapshot_disk_manifest_id, base_snapshot_disk_manifest_version,
                   base_snapshot_memory_manifest_id, base_snapshot_memory_manifest_version,
                   last_refreshed_at, created_at, updated_at, soft_deleted_at
              FROM enabled_images
             WHERE disk_manifest_id = $1
               AND disk_manifest_version = $2
               AND manifest_toml = $3
               AND base_snapshot_id IS NOT NULL
             ORDER BY COALESCE(updated_at, created_at) DESC
             LIMIT 1
            "#,
        )
        .bind(disk_manifest.manifest_id)
        .bind(disk_manifest.version as i64)
        .bind(manifest_toml)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| row::enabled_image_from_row(&r)).transpose()
    }

    // ---- enable jobs (ADR 0036) ----

    async fn create_or_get_enable_job(
        &self,
        image_uri: &str,
        manifest_digest: Option<&str>,
    ) -> Result<EnableJob, MetaError> {
        // INSERT guarded by the partial unique index (one non-terminal
        // job per image_uri); on conflict fall through to SELECTing
        // the in-flight job. Re-POST = resume, never duplicate work.
        let inserted = sqlx::query(
            r#"
            INSERT INTO enable_jobs (id, image_uri, manifest_digest)
            VALUES ($1, $2, $3)
            ON CONFLICT (image_uri) WHERE state NOT IN ('ready', 'failed')
            DO NOTHING
            RETURNING id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, created_at, updated_at
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(image_uri)
        .bind(manifest_digest)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        if let Some(row) = inserted {
            return row::enable_job_from_row(&row);
        }
        let existing = sqlx::query(
            r#"
            SELECT id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, created_at, updated_at
              FROM enable_jobs
             WHERE image_uri = $1 AND state NOT IN ('ready', 'failed')
            "#,
        )
        .bind(image_uri)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        match existing {
            Some(row) => row::enable_job_from_row(&row),
            // Raced with the active job reaching a terminal state
            // between INSERT and SELECT — retry the insert once.
            None => {
                let row = sqlx::query(
                    r#"
                    INSERT INTO enable_jobs (id, image_uri, manifest_digest)
                    VALUES ($1, $2, $3)
                    ON CONFLICT (image_uri) WHERE state NOT IN ('ready', 'failed')
                    DO NOTHING
                    RETURNING id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, created_at, updated_at
                    "#,
                )
                .bind(Uuid::new_v4())
                .bind(image_uri)
                .bind(manifest_digest)
                .fetch_optional(&self.pool)
                .await
                .map_err(db_err)?
                .ok_or_else(|| {
                    MetaError::Conflict(format!(
                        "enable job for `{image_uri}` raced two creates; retry"
                    ))
                })?;
                row::enable_job_from_row(&row)
            }
        }
    }

    async fn get_enable_job(&self, id: Uuid) -> Result<Option<EnableJob>, MetaError> {
        let row = sqlx::query(
            r#"SELECT id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, created_at, updated_at FROM enable_jobs WHERE id = $1"#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| row::enable_job_from_row(&r)).transpose()
    }

    async fn list_enable_jobs(&self, limit: u32) -> Result<Vec<EnableJob>, MetaError> {
        let rows = sqlx::query(
            r#"
            SELECT id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, created_at, updated_at
              FROM enable_jobs
             ORDER BY created_at DESC
             LIMIT $1
            "#,
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row::enable_job_from_row).collect()
    }

    async fn claim_enable_jobs(
        &self,
        claimant: &str,
        lease_secs: u32,
        limit: u32,
    ) -> Result<Vec<EnableJob>, MetaError> {
        // Atomic claim: stamp (claimed_by, claimed_at) on non-terminal
        // jobs whose lease is free or expired. The inner SELECT ...
        // FOR UPDATE SKIP LOCKED keeps two pods' simultaneous sweeps
        // from blocking on each other — each claims a disjoint set.
        let rows = sqlx::query(
            r#"
            UPDATE enable_jobs
               SET claimed_by = $1, claimed_at = NOW(), updated_at = NOW()
             WHERE id IN (
                   SELECT id FROM enable_jobs
                    WHERE state NOT IN ('ready', 'failed')
                      AND (claimed_at IS NULL OR claimed_at < NOW() - make_interval(secs => $2))
                    ORDER BY created_at
                    LIMIT $3
                      FOR UPDATE SKIP LOCKED
             )
            RETURNING id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, created_at, updated_at
            "#,
        )
        .bind(claimant)
        .bind(lease_secs as f64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row::enable_job_from_row).collect()
    }

    async fn update_enable_job_progress(
        &self,
        id: Uuid,
        chunks_done: u32,
        chunks_total: Option<u32>,
    ) -> Result<(), MetaError> {
        let res = sqlx::query(
            r#"
            UPDATE enable_jobs
               SET chunks_done = $2,
                   chunks_total = COALESCE($3, chunks_total),
                   claimed_at = NOW(),
                   updated_at = NOW()
             WHERE id = $1
            "#,
        )
        .bind(id)
        .bind(chunks_done as i32)
        .bind(chunks_total.map(|v| v as i32))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(MetaError::NotFound);
        }
        Ok(())
    }

    async fn set_enable_job_state(&self, id: Uuid, state: EnableJobState) -> Result<(), MetaError> {
        // Terminal states release the claim; non-failed targets clear
        // any stale error from a prior retried attempt.
        let res = sqlx::query(
            r#"
            UPDATE enable_jobs
               SET state = $2,
                   error = CASE WHEN $2 = 'failed' THEN error ELSE NULL END,
                   claimed_by = CASE WHEN $2 IN ('ready', 'failed') THEN NULL ELSE claimed_by END,
                   claimed_at = CASE WHEN $2 IN ('ready', 'failed') THEN NULL ELSE NOW() END,
                   updated_at = NOW()
             WHERE id = $1
            "#,
        )
        .bind(id)
        .bind(state.as_str())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(MetaError::NotFound);
        }
        Ok(())
    }

    async fn record_enable_job_failure(&self, id: Uuid, error: &str) -> Result<u32, MetaError> {
        // Release the claim so ANY pod's next tick can retry — the
        // failing pod holds no special ownership of the retry.
        let attempts: i32 = sqlx::query_scalar(
            r#"
            UPDATE enable_jobs
               SET attempts = attempts + 1,
                   error = $2,
                   claimed_by = NULL,
                   claimed_at = NULL,
                   updated_at = NOW()
             WHERE id = $1
            RETURNING attempts
            "#,
        )
        .bind(id)
        .bind(error)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or(MetaError::NotFound)?;
        Ok(attempts.max(0) as u32)
    }

    async fn retry_enable_job(&self, id: Uuid) -> Result<EnableJob, MetaError> {
        let row = sqlx::query(
            r#"
            UPDATE enable_jobs
               SET state = 'pending', attempts = 0, error = NULL,
                   claimed_by = NULL, claimed_at = NULL, updated_at = NOW()
             WHERE id = $1 AND state = 'failed'
            RETURNING id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, created_at, updated_at
            "#,
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        match row {
            Some(r) => row::enable_job_from_row(&r),
            None => match self.get_enable_job(id).await? {
                Some(job) => Err(MetaError::Conflict(format!(
                    "enable job {id} is `{}`, not `failed`; only failed jobs can be retried",
                    job.state.as_str()
                ))),
                None => Err(MetaError::NotFound),
            },
        }
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

    // ---- sealed secrets (ADR 0039 Task 13) ----

    async fn put_sealed_secret(
        &self,
        key: &str,
        wrapped_dek: Vec<u8>,
        nonce: Vec<u8>,
        ciphertext: Vec<u8>,
        key_id: String,
    ) -> Result<(), MetaError> {
        sqlx::query(
            r#"
            INSERT INTO sealed_secrets
                (key, wrapped_dek, nonce, ciphertext, key_id, created_at)
            VALUES ($1, $2, $3, $4, $5, NOW())
            ON CONFLICT (key) DO UPDATE SET
                wrapped_dek = EXCLUDED.wrapped_dek,
                nonce       = EXCLUDED.nonce,
                ciphertext  = EXCLUDED.ciphertext,
                key_id      = EXCLUDED.key_id,
                updated_at  = NOW()
            "#,
        )
        .bind(key)
        .bind(&wrapped_dek)
        .bind(&nonce)
        .bind(&ciphertext)
        .bind(&key_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn has_sealed_secret(&self, key: &str) -> Result<bool, MetaError> {
        let row = sqlx::query("SELECT 1 FROM sealed_secrets WHERE key = $1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(row.is_some())
    }

    async fn get_sealed_secret(
        &self,
        key: &str,
    ) -> Result<Option<engram_core::traits::SealedSecretRow>, MetaError> {
        let row = sqlx::query(
            "SELECT key, wrapped_dek, nonce, ciphertext, key_id FROM sealed_secrets WHERE key = $1",
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| {
            Ok(engram_core::traits::SealedSecretRow {
                key: r.try_get::<String, _>("key").map_err(db_err)?,
                wrapped_dek: r.try_get::<Vec<u8>, _>("wrapped_dek").map_err(db_err)?,
                nonce: r.try_get::<Vec<u8>, _>("nonce").map_err(db_err)?,
                ciphertext: r.try_get::<Vec<u8>, _>("ciphertext").map_err(db_err)?,
                key_id: r.try_get::<String, _>("key_id").map_err(db_err)?,
            })
        })
        .transpose()
    }

    async fn delete_sealed_secret(&self, key: &str) -> Result<(), MetaError> {
        sqlx::query("DELETE FROM sealed_secrets WHERE key = $1")
            .bind(key)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    // ----------------------------------------------------------------
    // ADR 0016 §A.1.5c — session_lease leasing row.
    // ----------------------------------------------------------------

    async fn try_acquire_session_lease(
        &self,
        session_id: SessionId,
        sandbox_id: Option<engram_core::SandboxId>,
        locked_by: &str,
    ) -> Result<bool, MetaError> {
        // ON CONFLICT (session_id) DO NOTHING returns 0 rows
        // affected when the row already exists. Atomic vs. a
        // racing INSERT from another coord pod.
        let res = sqlx::query(
            "INSERT INTO session_lease (session_id, locked_by, sandbox_id) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (session_id) DO NOTHING",
        )
        .bind(session_id.as_uuid())
        .bind(locked_by)
        .bind(sandbox_id.map(|s| s.as_uuid()))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() == 1)
    }

    async fn release_session_lease(&self, session_id: SessionId) -> Result<(), MetaError> {
        sqlx::query("DELETE FROM session_lease WHERE session_id = $1")
            .bind(session_id.as_uuid())
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        // Idempotent: missing row = already released / never held.
        Ok(())
    }

    async fn touch_session_lease(
        &self,
        session_id: SessionId,
        locked_by: &str,
    ) -> Result<bool, MetaError> {
        let res = sqlx::query(
            "UPDATE session_lease SET locked_at = now() \
             WHERE session_id = $1 AND locked_by = $2",
        )
        .bind(session_id.as_uuid())
        .bind(locked_by)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() == 1)
    }

    async fn sweep_stale_session_leases(
        &self,
        max_age: std::time::Duration,
    ) -> Result<Vec<engram_core::traits::StaleSessionLease>, MetaError> {
        // DELETE ... RETURNING is one statement; rows lifted to
        // app-side for warn-logging. Interval is passed as seconds
        // (BIGINT-castable) because the sqlx postgres driver doesn't
        // bind `std::time::Duration` natively.
        let max_age_secs = max_age.as_secs() as i64;
        let rows = sqlx::query_as::<
            _,
            (
                uuid::Uuid,
                Option<uuid::Uuid>,
                String,
                chrono::DateTime<chrono::Utc>,
            ),
        >(
            "DELETE FROM session_lease \
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
                engram_core::traits::StaleSessionLease {
                    session_id: SessionId::from(session_id),
                    sandbox_id: sandbox_id.map(engram_core::SandboxId::from),
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
    ///
    /// ADR 0021 P1.8: soft-deleted rows (`soft_deleted_at IS NOT NULL`)
    /// **are intentionally included** in the pin set. The soft-delete
    /// is the chunk-lineage extension mechanism — a row stays in PG
    /// (and contributes its disk-manifest chunks here) until the
    /// future refcount-driven physical delete drops it. This keeps the
    /// resume path correctness invariant — "if the row exists, its
    /// chunks are reachable" — without needing a separate "soft-
    /// deleted-but-still-referenced" pin source.
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

    /// ADR 0022 Option A: pin-set source #5 — every enabled image's
    /// base-snapshot MEMORY manifest (the per-template base memfile's
    /// backing). Pins independent of the base snapshot row's `recoverable`
    /// flag and of `soft_deleted_at` (mirrors source #1). `enabled_images`
    /// is tens of rows per deployment, so a plain DISTINCT scan is well
    /// under a millisecond — no index needed (cf. migration 0041).
    async fn list_enabled_image_base_snapshot_memory_manifests(
        &self,
    ) -> Result<Vec<engram_core::types::manifest::ManifestRef>, MetaError> {
        let rows = sqlx::query_as::<_, (Uuid, i64)>(
            "SELECT DISTINCT base_snapshot_memory_manifest_id, base_snapshot_memory_manifest_version
               FROM enabled_images
              WHERE base_snapshot_memory_manifest_id IS NOT NULL
                AND base_snapshot_memory_manifest_version IS NOT NULL",
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

    /// ADR 0022 Option A: pin-set source #6 — the disk companion to #5
    /// (every enabled image's base-snapshot DISK manifest, migration
    /// 0042). Same recoverable/soft-delete-independent semantics.
    async fn list_enabled_image_base_snapshot_disk_manifests(
        &self,
    ) -> Result<Vec<engram_core::types::manifest::ManifestRef>, MetaError> {
        let rows = sqlx::query_as::<_, (Uuid, i64)>(
            "SELECT DISTINCT base_snapshot_disk_manifest_id, base_snapshot_disk_manifest_version
               FROM enabled_images
              WHERE base_snapshot_disk_manifest_id IS NOT NULL
                AND base_snapshot_disk_manifest_version IS NOT NULL",
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

    /// ADR 0035 §5: distinct bundle generations referenced by any
    /// snapshot row. jsonb unnest in SQL so the coord never pages the
    /// whole table; the result is at most a handful of refs.
    async fn bundle_pin_set(
        &self,
    ) -> Result<Vec<engram_core::types::sandbox::AuxBundleRef>, MetaError> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT DISTINCT b->>'drive_id', b->>'sha256'
               FROM snapshots, jsonb_array_elements(aux_bundles) AS b",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let mut out: Vec<_> = rows
            .into_iter()
            .map(
                |(drive_id, sha256)| engram_core::types::sandbox::AuxBundleRef { drive_id, sha256 },
            )
            .collect();
        out.sort_by(|a, b| (&a.drive_id, &a.sha256).cmp(&(&b.drive_id, &b.sha256)));
        Ok(out)
    }

    /// ADR 0035 §5: sticky-first-seen candidate upsert (bundle
    /// flavor of `upsert_chunk_gc_candidate`).
    async fn upsert_bundle_gc_candidate(&self, sha256: &str) -> Result<(), MetaError> {
        sqlx::query(
            "INSERT INTO bundle_gc_candidates (sha256)
             VALUES ($1)
             ON CONFLICT (sha256) DO UPDATE SET last_seen_at = now()",
        )
        .bind(sha256)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// ADR 0035 §5 promote-pass query.
    async fn list_expired_bundle_gc_candidates(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
        limit: i64,
    ) -> Result<Vec<String>, MetaError> {
        sqlx::query_scalar::<_, String>(
            "SELECT sha256
               FROM bundle_gc_candidates
              WHERE first_seen_at < $1
              ORDER BY first_seen_at
              LIMIT $2",
        )
        .bind(cutoff)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)
    }

    /// ADR 0035 §5: batch-delete candidate rows.
    async fn delete_bundle_gc_candidates(&self, sha256s: &[String]) -> Result<(), MetaError> {
        if sha256s.is_empty() {
            return Ok(());
        }
        sqlx::query("DELETE FROM bundle_gc_candidates WHERE sha256 = ANY($1::text[])")
            .bind(sha256s)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// ADR 0028 addendum: the snapshot-blob pin set — every snapshot id
    /// with a live row. NO `recoverable`/`session_id` filter (see the
    /// trait doc): base/template rows (`session_id IS NULL`) must pin
    /// their blobs too, and a demoted row's blobs stay pinned until the
    /// row is pruned. Bounded (rows pruned by checkpoint retention), so
    /// no pagination — same posture as `bundle_pin_set`.
    async fn snapshot_blob_pin_set(
        &self,
    ) -> Result<Vec<engram_core::types::SnapshotId>, MetaError> {
        let ids = sqlx::query_scalar::<_, Uuid>("SELECT id FROM snapshots")
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(ids
            .into_iter()
            .map(engram_core::types::SnapshotId::from)
            .collect())
    }

    /// ADR 0028 addendum: sticky-first-seen candidate upsert (snapshot
    /// flavor of `upsert_bundle_gc_candidate`).
    async fn upsert_snapshot_blob_gc_candidate(
        &self,
        id: engram_core::types::SnapshotId,
    ) -> Result<(), MetaError> {
        sqlx::query(
            "INSERT INTO snapshot_blob_gc_candidates (snapshot_id)
             VALUES ($1)
             ON CONFLICT (snapshot_id) DO UPDATE SET last_seen_at = now()",
        )
        .bind(id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// ADR 0028 addendum: promote-pass query (oldest first, batched).
    async fn list_expired_snapshot_blob_gc_candidates(
        &self,
        cutoff: chrono::DateTime<chrono::Utc>,
        limit: i64,
    ) -> Result<Vec<engram_core::types::SnapshotId>, MetaError> {
        let ids = sqlx::query_scalar::<_, Uuid>(
            "SELECT snapshot_id
               FROM snapshot_blob_gc_candidates
              WHERE first_seen_at < $1
              ORDER BY first_seen_at
              LIMIT $2",
        )
        .bind(cutoff)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(ids
            .into_iter()
            .map(engram_core::types::SnapshotId::from)
            .collect())
    }

    /// ADR 0028 addendum: batch-delete candidate rows.
    async fn delete_snapshot_blob_gc_candidates(
        &self,
        ids: &[engram_core::types::SnapshotId],
    ) -> Result<(), MetaError> {
        if ids.is_empty() {
            return Ok(());
        }
        let uuids: Vec<Uuid> = ids.iter().map(|id| id.as_uuid()).collect();
        sqlx::query("DELETE FROM snapshot_blob_gc_candidates WHERE snapshot_id = ANY($1::uuid[])")
            .bind(&uuids)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// ADR 0029: one cheap aggregate over `snapshots` for the Storage
    /// surface's `snapshots` + `snapshot_bytes` rollups.
    async fn snapshot_totals(&self) -> Result<engram_core::traits::SnapshotTotals, MetaError> {
        let (count, total_bytes): (i64, i64) = sqlx::query_as(
            "SELECT count(*)::bigint, coalesce(sum(size_bytes), 0)::bigint FROM snapshots",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(engram_core::traits::SnapshotTotals {
            count: count.max(0) as u64,
            total_bytes: total_bytes.max(0) as u64,
        })
    }

    /// ADR 0029: count of chunks parked in `chunk_gc_candidates`
    /// awaiting their grace window — the Storage surface's "gc pending".
    async fn count_gc_candidates(&self) -> Result<u64, MetaError> {
        let count: i64 = sqlx::query_scalar("SELECT count(*)::bigint FROM chunk_gc_candidates")
            .fetch_one(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(count.max(0) as u64)
    }
}

// ADR 0039 final cleanup: `impl UserStore` and `impl WebSessionStore` removed.
// Human identity (users/web_sessions tables) moved to the orchestrator (ADR 0039 §5).
