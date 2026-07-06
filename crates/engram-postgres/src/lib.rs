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
use engram_core::traits::{
    CreateDisposition, DisableEnabledImageOutcome, MetadataStore, SessionCreateWriteSet,
};
use engram_core::types::{
    ArtifactRow, Capability, EnableJob, EnableJobState, EnabledImage, HostRecord, HostStatus,
    PersistedEvent, RegistryCredential, Session, SessionSecrets, SessionSpec, SessionState,
    SnapshotRecord,
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

    /// Issue #211: disambiguate a 0-row guarded UPDATE. If the row exists
    /// at all, the guard (status / expected-sandbox CAS) rejected the
    /// write — a `Conflict`. Otherwise the id is genuinely unknown —
    /// `NotFound`. Mirrors the shape `transition_session` uses.
    async fn conflict_or_not_found(&self, id: SessionId) -> MetaError {
        match sqlx::query_scalar::<_, i64>("SELECT 1::bigint FROM sessions WHERE id = $1")
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await
        {
            Ok(Some(_)) => MetaError::Conflict(format!(
                "guarded session write rejected for {id}: state/sandbox precondition not met"
            )),
            Ok(None) => MetaError::NotFound,
            Err(e) => db_err(e),
        }
    }

    /// Issue #232: disambiguate a 0-row fenced enable-job UPDATE. If the
    /// row exists, the `claimed_by = $claimant` fence rejected the write
    /// — the lease has expired and a peer (or nobody) now holds the
    /// claim, so this is a `Conflict` and the caller must abandon the
    /// job. Otherwise the id is genuinely unknown — `NotFound`. Mirrors
    /// [`Self::conflict_or_not_found`].
    async fn enable_job_fence_miss(&self, id: Uuid, claimant: &str) -> MetaError {
        match sqlx::query_scalar::<_, Option<String>>(
            "SELECT claimed_by FROM enable_jobs WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        {
            Ok(Some(holder)) => MetaError::Conflict(format!(
                "enable job {id} lease lost: held by {} (writer is {claimant})",
                holder.as_deref().unwrap_or("<unclaimed>")
            )),
            Ok(None) => MetaError::NotFound,
            Err(e) => db_err(e),
        }
    }

    /// ADR 0048 (queue fairness): best-effort wake for `queue_scanner`,
    /// fired at every discrete placement-feasibility event (a reservation
    /// freed, a `pending` reservation released, a host (re)registered or
    /// uncordoned, a session freshly enqueued). The scanner LISTENs on
    /// `placement_changed` and retries immediately instead of waiting for
    /// its fallback poll (`ENGRAM_QUEUE_POLL_SECS`). Mirrors the
    /// `org_secret_changed` precedent above: best-effort `let _ =`, the
    /// payload is an informational reason string only, and a NOTIFY
    /// failure must never fail (or roll back) the caller's write — the
    /// poll fallback is the durability story, not this.
    async fn notify_placement_changed(&self, reason: &str) {
        let _ = sqlx::query("SELECT pg_notify('placement_changed', $1)")
            .bind(reason)
            .execute(&self.pool)
            .await;
    }
}

fn db_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> MetaError {
    MetaError::Db(Box::new(e))
}

/// ADR 0055 P2: a `mount_catalog` row → the shared `CatalogSkill`. The tuple is
/// `(id, owner, name, description, sha256, mount_json, size_bytes, created_at)`.
#[allow(clippy::type_complexity)]
fn catalog_skill_from_row(
    row: (
        String,
        String,
        String,
        String,
        String,
        String,
        i64,
        chrono::DateTime<chrono::Utc>,
    ),
) -> engram_core::types::CatalogSkill {
    let (id, owner, name, description, sha256, mount_json, size_bytes, created_at) = row;
    engram_core::types::CatalogSkill {
        id,
        owner,
        name,
        description,
        sha256,
        mount_json,
        size_bytes,
        created_at,
    }
}

/// ADR 0062: a `harness_catalog` row → the shared `CatalogHarness`. The tuple is
/// `(id, owner, name, oci_ref, manifest_digest, descriptor_toml, squashfs_sha256,
/// squashfs_size_bytes, created_at)`.
type HarnessRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    chrono::DateTime<chrono::Utc>,
);

fn catalog_harness_from_row(row: HarnessRow) -> engram_core::types::CatalogHarness {
    let (
        id,
        owner,
        name,
        oci_ref,
        manifest_digest,
        descriptor_toml,
        squashfs_sha256,
        squashfs_size_bytes,
        created_at,
    ) = row;
    engram_core::types::CatalogHarness {
        id,
        owner,
        name,
        oci_ref,
        manifest_digest,
        descriptor_toml,
        squashfs_sha256,
        squashfs_size_bytes,
        created_at,
    }
}

/// The `RETURNING`/`SELECT` column list for a `harness_catalog` row, in
/// [`HarnessRow`] order.
const HARNESS_ROW_COLS: &str =
    "id, owner, name, oci_ref, manifest_digest, descriptor_toml, squashfs_sha256, squashfs_size_bytes, created_at";

/// ADR 0048: per-host placement inputs. `alloc_mib` is the host-measured
/// RAM headroom (`<= 0` = unmeasured); `cpu_budget` is `total_vcpus ×
/// overcommit` (`0` = host hasn't reported its core count → no CPU gate).
#[derive(Clone, Copy, Debug, Default)]
struct HostFit {
    alloc_mib: i64,
    reserved_mib: i64,
    cpu_budget: i64,
    reserved_vcpus: i64,
}

/// ADR 0046/0048: BEST-FIT, 2D placement among `candidates` (ranked, with the
/// snapshot-affinity hosts forming the first `affinity_len`). Packs onto the
/// host with the SMALLEST free RAM that still fits BOTH dimensions
/// (`free_mib ≥ budget_mib AND free_vcpus ≥ budget_vcpus`) — best-fit so
/// scale-down pressure surfaces instead of spreading. The affinity prefix is a
/// real tier: a fitting affinity host wins over a tighter non-affinity host.
/// Unmeasured-RAM hosts (dev / brand-new) are a last-resort fallback spanning
/// both tiers. `None` → nothing fits → caller rejects / queues.
///
/// Pure (no I/O) so it's unit-tested without a database.
fn choose_placement_host(
    candidates: &[uuid::Uuid],
    affinity_len: usize,
    fit: &std::collections::HashMap<uuid::Uuid, HostFit>,
    budget_mib: i64,
    budget_vcpus: i64,
) -> Option<uuid::Uuid> {
    let split = affinity_len.min(candidates.len());
    best_fit_measured(&candidates[..split], fit, budget_mib, budget_vcpus)
        .or_else(|| best_fit_measured(&candidates[split..], fit, budget_mib, budget_vcpus))
        // Last resort across BOTH tiers: a host with no allocatable
        // measurement yet (don't gate on a bogus 0; let dev/new hosts work).
        .or_else(|| {
            candidates
                .iter()
                .find(|h| fit.get(h).is_some_and(|f| f.alloc_mib <= 0))
                .copied()
        })
}

/// Best-fit (smallest free RAM that fits both dims) among MEASURED hosts.
fn best_fit_measured(
    candidates: &[uuid::Uuid],
    fit: &std::collections::HashMap<uuid::Uuid, HostFit>,
    budget_mib: i64,
    budget_vcpus: i64,
) -> Option<uuid::Uuid> {
    let mut best: Option<(i64, uuid::Uuid)> = None; // (free_mib, host)
    for h in candidates {
        let Some(f) = fit.get(h) else {
            continue; // not ready/draining at lock time
        };
        if f.alloc_mib <= 0 {
            continue; // unmeasured — handled by the fallback tier
        }
        let free_mib = f.alloc_mib - f.reserved_mib;
        if free_mib < budget_mib {
            continue;
        }
        // CPU dimension: only gate when the host reported a budget.
        if f.cpu_budget > 0 && f.cpu_budget - f.reserved_vcpus < budget_vcpus {
            continue;
        }
        // SMALLEST free that fits (best-fit); ties toward the earlier
        // (higher-ranked) candidate.
        match best {
            Some((bf, _)) if bf <= free_mib => {}
            _ => best = Some((free_mib, *h)),
        }
    }
    best.map(|(_, h)| h)
}

// Kept beside `choose_placement_host` (the fn it exercises) rather than at the
// file end — the `MetadataStore` impl follows.
#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod placement_tests {
    use super::{choose_placement_host, HostFit};
    use std::collections::HashMap;
    use uuid::Uuid;

    fn ids(n: usize) -> Vec<Uuid> {
        (1..=n as u128).map(Uuid::from_u128).collect()
    }

    /// Build a fit map from (alloc_mib, reserved_mib) pairs; CPU budget
    /// left at 0 (= no CPU gate) unless a test overrides it.
    fn ram_fit(entries: &[(Uuid, i64, i64)]) -> HashMap<Uuid, HostFit> {
        entries
            .iter()
            .map(|&(id, alloc, reserved)| {
                (
                    id,
                    HostFit {
                        alloc_mib: alloc,
                        reserved_mib: reserved,
                        cpu_budget: 0,
                        reserved_vcpus: 0,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn best_fit_packs_onto_the_tightest_host_that_fits() {
        // h0 has LESS free (4768) than h1 (32768); best-fit packs h0.
        let h = ids(2);
        let fit = ram_fit(&[(h[0], 32768, 28000), (h[1], 32768, 0)]);
        assert_eq!(choose_placement_host(&h, 0, &fit, 4096, 0), Some(h[0]));
    }

    #[test]
    fn rejects_when_none_fit() {
        let h = ids(2);
        let fit = ram_fit(&[(h[0], 8192, 6000), (h[1], 8192, 6000)]);
        assert_eq!(choose_placement_host(&h, 0, &fit, 4096, 0), None);
    }

    #[test]
    fn cpu_dimension_binds_before_ram() {
        // Both hosts have ample RAM, but h0's CPU budget is exhausted
        // (8 budget − 8 reserved = 0 free vCPU) so a 2-vCPU session must
        // land on h1 despite h0 being the tighter RAM fit.
        let h = ids(2);
        let fit: HashMap<_, _> = [
            (
                h[0],
                HostFit {
                    alloc_mib: 32768,
                    reserved_mib: 28000,
                    cpu_budget: 8,
                    reserved_vcpus: 8,
                },
            ),
            (
                h[1],
                HostFit {
                    alloc_mib: 32768,
                    reserved_mib: 0,
                    cpu_budget: 32,
                    reserved_vcpus: 0,
                },
            ),
        ]
        .into();
        assert_eq!(choose_placement_host(&h, 0, &fit, 4096, 2), Some(h[1]));
    }

    #[test]
    fn unknown_allocatable_is_a_fallback_not_a_gate() {
        let h = ids(2);
        // h0 unmeasured (0), h1 measured + fits → prefer the measured host.
        let fit = ram_fit(&[(h[0], 0, 0), (h[1], 32768, 0)]);
        assert_eq!(choose_placement_host(&h, 0, &fit, 4096, 0), Some(h[1]));
        // only the unmeasured host (dev backend / brand-new) → fall back to it.
        let only0 = ram_fit(&[(h[0], 0, 0)]);
        assert_eq!(
            choose_placement_host(&h[..1], 0, &only0, 4096, 0),
            Some(h[0])
        );
    }

    #[test]
    fn affinity_prefix_wins_over_a_tighter_non_affinity_host() {
        // h0 is the affinity host (affinity_len=1) with MORE free RAM;
        // best-fit would otherwise prefer the tighter h1, but the
        // affinity tier is tried first and h0 fits.
        let h = ids(2);
        let fit = ram_fit(&[(h[0], 32768, 0), (h[1], 32768, 28000)]);
        assert_eq!(choose_placement_host(&h, 1, &fit, 4096, 0), Some(h[0]));
        // ...but if the affinity host can't fit, fall through to best-fit
        // over the remainder.
        let full_affinity = ram_fit(&[(h[0], 8192, 8000), (h[1], 32768, 28000)]);
        assert_eq!(
            choose_placement_host(&h, 1, &full_affinity, 4096, 0),
            Some(h[1])
        );
    }

    /// The scenario this whole ADR exists for: a burst of 4 GiB sessions
    /// onto two ~16 GiB hosts now PACKS one host full before spilling to
    /// the next (best-fit), instead of spreading — so scale-down has a
    /// fully-idle host to shed.
    #[test]
    fn burst_packs_one_host_then_overflows_to_next() {
        let h = ids(2);
        let mut reserved: HashMap<Uuid, i64> = HashMap::new();
        let budget = 4096;
        let mut picks = Vec::new();
        for _ in 0..10 {
            let fit = ram_fit(&[
                (h[0], 16384, reserved.get(&h[0]).copied().unwrap_or(0)),
                (h[1], 16384, reserved.get(&h[1]).copied().unwrap_or(0)),
            ]);
            match choose_placement_host(&h, 0, &fit, budget, 0) {
                Some(p) => {
                    *reserved.entry(p).or_default() += budget;
                    picks.push(Some(p));
                }
                None => picks.push(None),
            }
        }
        let placed = picks.iter().flatten().count();
        assert_eq!(placed, 8, "16384/4096 = 4 per host = 8 total fit");
        // h0 fills completely (4 sessions) before h1 takes any — packing.
        let on0 = picks.iter().flatten().filter(|&&p| p == h[0]).count();
        assert_eq!(on0, 4, "best-fit packs h0 full before spilling to h1");
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

    async fn transition_session_created(
        &self,
        session_id: SessionId,
        sandbox_id: SandboxId,
    ) -> Result<(), MetaError> {
        // Issue #535 (c): the row is GUARANTEED to already exist (`pending`,
        // committed by `reserve_and_persist_create` before any host RPC ran)
        // — a slim UPDATE replaces the old `create_session_created` upsert.
        // But existing != still-`pending`: DeleteSession can remove the row,
        // or `requeue_stale_pending` can flip it back to `queued`, while the
        // restore RPC that preceded this call is in flight. Guard on the
        // expected state and check `rows_affected` so a lost race surfaces
        // as `NotFound` instead of silently binding `sandbox_id` onto
        // whatever status the row now has — the caller's `Err` arm tears the
        // now-orphaned sandbox back down.
        let res = sqlx::query(
            r#"
            UPDATE sessions
               SET status = 'created', sandbox_id = $2, last_active_at = NOW()
             WHERE id = $1 AND status = 'pending'
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(sandbox_id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(MetaError::NotFound);
        }
        Ok(())
    }

    async fn reserve_and_persist_create(
        &self,
        ws: SessionCreateWriteSet,
        candidates: &[HostId],
        affinity_len: usize,
    ) -> Result<CreateDisposition, MetaError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        // -------- pick a host (ADR 0046/0048 best-fit 2D), if any candidate --------
        // Issue #535 (b): this is `reserve_placement`'s FOR-UPDATE pick, kept
        // verbatim — extended below so the SAME transaction also writes the
        // satellites instead of stopping at the bare row insert.
        let cand: Vec<uuid::Uuid> = candidates.iter().map(|h| h.as_uuid()).collect();
        let picked: Option<uuid::Uuid> = if cand.is_empty() {
            None
        } else {
            // Lock the candidate host rows so concurrent placers (any coord
            // replica) serialize on the overlap — a burst can't read the same
            // pre-insert reserved figure and stack onto one host. Issue #535
            // (b): unlike the old `reserve_placement`, this lock is now held
            // for the REST of the transaction, not just the pick + insert —
            // `tx.commit()` is at the bottom of this function, after the
            // sealed-secrets insert, the per-capability insert loop, and the
            // integration-policy upsert all run on the same `tx`. A
            // many-capability create serializes concurrent placers on that
            // whole multi-round-trip critical section, not a sub-ms window.
            let host_rows = sqlx::query(
                r#"
                SELECT id, allocatable_mib, total_vcpus
                FROM hosts
                WHERE id = ANY($1) AND status IN ('ready','draining') AND NOT cordoned
                -- ORDER BY id BEFORE `FOR UPDATE`: every placer (any replica,
                -- both this and `place_queued_session`) locks the overlapping
                -- host rows in the SAME (PK) order, so a burst can't lock
                -- {A,B} vs {B,A} and deadlock. The LockRows executor node sits
                -- atop the sort, so rows are locked in id order. (Load test:
                -- `deadlock detected` under concurrent creates before this.)
                ORDER BY id
                FOR UPDATE
                "#,
            )
            .bind(&cand)
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            // ADR 0046/0048: build the 2D fit map. allocatable_mib is the
            // host-measured RAM headroom (nets out daemon/OS/chunk-cache/mlock
            // baseline; 0 = unmeasured). The CPU budget is total_vcpus ×
            // overcommit (0 = host hasn't reported its core count → no CPU gate).
            let mut fit: std::collections::HashMap<uuid::Uuid, HostFit> =
                std::collections::HashMap::with_capacity(host_rows.len());
            for r in &host_rows {
                let id: uuid::Uuid = sqlx::Row::try_get(r, "id").map_err(db_err)?;
                let alloc_mib: i64 = sqlx::Row::try_get(r, "allocatable_mib").map_err(db_err)?;
                let total_vcpus: i32 = sqlx::Row::try_get(r, "total_vcpus").map_err(db_err)?;
                fit.insert(
                    id,
                    HostFit {
                        alloc_mib,
                        reserved_mib: 0,
                        cpu_budget: engram_core::types::host::host_cpu_budget(
                            total_vcpus.max(0) as u32
                        ),
                        reserved_vcpus: 0,
                    },
                );
            }
            // Reserved within the txn — sees the committed `pending` rows of
            // placers that locked these hosts before us. Status list is the
            // SQL twin of `SessionState::host_memory_reserving_states()`.
            let res_rows = sqlx::query(
                r#"
                SELECT host_id,
                       COALESCE(SUM(mem_budget_mib), 0)::BIGINT AS reserved_mib,
                       COALESCE(SUM(cpu_budget_vcpus), 0)::BIGINT AS reserved_vcpus
                FROM sessions
                WHERE host_id = ANY($1)
                  AND status IN ('pending','created','active',
                                 'evacuating','evicting')
                  -- A `pending` row older than 10 min is a crash-orphaned
                  -- reservation (a boot never takes that long); don't let it
                  -- leak into the reserved figure and false-reject the host.
                  -- ADR 0048: gate on last_active_at, not created_at — a
                  -- session can sit `queued` for many minutes before
                  -- `place_queued_session` flips it to `pending` (bumping
                  -- last_active_at), and an old created_at would make that
                  -- fresh reservation look crash-orphaned and leak
                  -- (overcommit). This call sets both to NOW().
                  AND (status <> 'pending' OR last_active_at > NOW() - INTERVAL '10 minutes')
                GROUP BY host_id
                "#,
            )
            .bind(&cand)
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            for r in &res_rows {
                let h: uuid::Uuid = sqlx::Row::try_get(r, "host_id").map_err(db_err)?;
                let mem: i64 = sqlx::Row::try_get(r, "reserved_mib").map_err(db_err)?;
                let cpu: i64 = sqlx::Row::try_get(r, "reserved_vcpus").map_err(db_err)?;
                if let Some(f) = fit.get_mut(&h) {
                    f.reserved_mib = mem;
                    f.reserved_vcpus = cpu;
                }
            }
            // Best-fit, 2D, affinity-prefix-first among the ranked candidates
            // — see `choose_placement_host` (unit-tested).
            choose_placement_host(
                &cand,
                affinity_len,
                &fit,
                ws.mem_budget_mib,
                ws.cpu_budget_vcpus as i64,
            )
        };

        let now = Utc::now();
        let disposition = match picked {
            Some(host) => {
                sqlx::query(
                    r#"
                    INSERT INTO sessions
                        (id, status, host_id, sandbox_id,
                         image_uri, mode, mem_budget_mib, cpu_budget_vcpus,
                         harness, selected_skills, queue_prompt,
                         created_at, last_active_at)
                    VALUES ($1, 'pending', $2, NULL, $3, $4, $5, $6, $7, $8, $9, $10, $10)
                    "#,
                )
                .bind(ws.session_id.as_uuid())
                .bind(host)
                .bind(&ws.spec.image)
                .bind(ws.spec.mode.as_str())
                .bind(ws.mem_budget_mib)
                .bind(ws.cpu_budget_vcpus)
                .bind(ws.selected_harness.as_deref())
                .bind(&ws.selected_skills)
                .bind(ws.queue_prompt.as_deref())
                .bind(now)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                CreateDisposition::Placed(HostId(host))
            }
            None => {
                // ADR 0048: no host fits (or there were no candidates at
                // all) → the SAME transaction inserts the row `queued`
                // instead — the enqueue path is no longer a second copy.
                sqlx::query(
                    r#"
                    INSERT INTO sessions
                        (id, status, host_id, sandbox_id, image_uri, mode,
                         mem_budget_mib, cpu_budget_vcpus,
                         harness, selected_skills,
                         queued_at, queue_origin, queue_prompt,
                         created_at, last_active_at)
                    VALUES ($1, 'queued', NULL, NULL, $2, $3, $4, $5, $6, $7, $8, 'create', $9, $8, $8)
                    "#,
                )
                .bind(ws.session_id.as_uuid())
                .bind(&ws.spec.image)
                .bind(ws.spec.mode.as_str())
                .bind(ws.mem_budget_mib)
                .bind(ws.cpu_budget_vcpus)
                .bind(ws.selected_harness.as_deref())
                .bind(&ws.selected_skills)
                .bind(now)
                .bind(ws.queue_prompt.as_deref())
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                CreateDisposition::Queued
            }
        };

        // -------- satellites, in the SAME transaction (issue #535 (b)) --------
        // `harness` and `selected_skills` already rode the row INSERT above;
        // the remaining satellites keep their own tables (FK'd to
        // `sessions.id`, now guaranteed to exist by the time this commits).
        if let Some(secrets) = ws.sealed_secrets {
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
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        // ON CONFLICT DO NOTHING keeps a re-persist idempotent; `resource`
        // '' is the no-resource sentinel (mirrors `bind_session_capabilities`).
        for c in &ws.capabilities {
            sqlx::query(
                r#"
                INSERT INTO session_capabilities (session_id, provider, action, resource)
                VALUES ($1, $2, $3, $4)
                ON CONFLICT DO NOTHING
                "#,
            )
            .bind(ws.session_id.as_uuid())
            .bind(&c.provider)
            .bind(&c.action)
            .bind(c.resource.as_deref().unwrap_or(""))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        if let Some(policy_json) = ws.integration_policy_json.as_deref() {
            sqlx::query(
                r#"
                INSERT INTO session_integration_policy (session_id, policy_json)
                VALUES ($1, $2)
                ON CONFLICT (session_id) DO UPDATE SET policy_json = EXCLUDED.policy_json
                "#,
            )
            .bind(ws.session_id.as_uuid())
            .bind(policy_json)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }

        tx.commit().await.map_err(db_err)?;
        if matches!(disposition, CreateDisposition::Queued) {
            // Closes the race where capacity frees between the failed
            // in-transaction pick and this commit — without this, the
            // newly-queued row would wait for the next poll fallback even
            // though a host was free the whole time. This is the ADR 0048
            // NOTIFY that used to live in the now-retired standalone
            // `enqueue_session_create`; `reserve_and_persist_create`
            // subsumed that function (issue #535 (b)) so it fires here.
            self.notify_placement_changed("enqueued").await;
        }
        Ok(disposition)
    }

    async fn set_teleport_target(
        &self,
        id: SessionId,
        target: Option<HostId>,
    ) -> Result<(), MetaError> {
        // Issue #214: stamp `teleport_target_set_at` whenever a pin is
        // set (NOW()), and clear it when the pin is cleared (Some→non-NULL,
        // None→NULL), so the two columns are always consistent. The scanner
        // ages out a stale pin off this timestamp.
        sqlx::query(
            "UPDATE sessions \
             SET teleport_target_host_id = $2, \
                 teleport_target_set_at = CASE WHEN $2 IS NULL THEN NULL ELSE NOW() END \
             WHERE id = $1",
        )
        .bind(id.as_uuid())
        .bind(target.map(|h| h.as_uuid()))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_teleport_target(
        &self,
        id: SessionId,
    ) -> Result<Option<(HostId, Option<chrono::DateTime<chrono::Utc>>)>, MetaError> {
        let row: Option<(Option<uuid::Uuid>, Option<chrono::DateTime<chrono::Utc>>)> =
            sqlx::query_as(
                "SELECT teleport_target_host_id, teleport_target_set_at \
                 FROM sessions WHERE id = $1",
            )
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(row.and_then(|(t, set_at)| t.map(|u| (HostId(u), set_at))))
    }

    async fn insert_broker_token(
        &self,
        token: engram_core::types::registry::SessionBrokerToken,
    ) -> Result<bool, MetaError> {
        // First-writer-wins: a sibling replica racing the mint loses
        // cleanly and re-reads the winner's row.
        let n = sqlx::query(
            r#"
            INSERT INTO session_broker_tokens
                (session_id, wrapped_dek, nonce, ciphertext, key_id)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (session_id) DO NOTHING
            "#,
        )
        .bind(token.session_id.as_uuid())
        .bind(&token.wrapped_dek)
        .bind(&token.nonce)
        .bind(&token.ciphertext)
        .bind(&token.key_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?
        .rows_affected();
        Ok(n == 1)
    }

    async fn get_broker_token(
        &self,
        id: SessionId,
    ) -> Result<Option<engram_core::types::registry::SessionBrokerToken>, MetaError> {
        let row: Option<(Vec<u8>, Vec<u8>, Vec<u8>, String)> = sqlx::query_as(
            r#"
            SELECT wrapped_dek, nonce, ciphertext, key_id
            FROM session_broker_tokens WHERE session_id = $1
            "#,
        )
        .bind(id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(row.map(|(wrapped_dek, nonce, ciphertext, key_id)| {
            engram_core::types::registry::SessionBrokerToken {
                session_id: id,
                wrapped_dek,
                nonce,
                ciphertext,
                key_id,
            }
        }))
    }

    async fn delete_broker_token(&self, id: SessionId) -> Result<(), MetaError> {
        sqlx::query("DELETE FROM session_broker_tokens WHERE session_id = $1")
            .bind(id.as_uuid())
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    // ---- ADR 0057: org secret store (KEK-sealed, admin-managed) ----

    async fn upsert_org_secret(
        &self,
        sealed: engram_core::types::org_secret::SealedOrgSecret,
    ) -> Result<engram_core::types::org_secret::OrgSecret, MetaError> {
        let (name, key_id, created_at, updated_at): (
            String,
            String,
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
        ) = sqlx::query_as(
            r#"
            INSERT INTO org_secrets (name, wrapped_dek, nonce, ciphertext, key_id, updated_at)
            VALUES ($1, $2, $3, $4, $5, now())
            ON CONFLICT (name) DO UPDATE SET
                wrapped_dek = EXCLUDED.wrapped_dek,
                nonce       = EXCLUDED.nonce,
                ciphertext  = EXCLUDED.ciphertext,
                key_id      = EXCLUDED.key_id,
                updated_at  = now()
            RETURNING name, key_id, created_at, updated_at
            "#,
        )
        .bind(&sealed.name)
        .bind(&sealed.wrapped_dek)
        .bind(&sealed.nonce)
        .bind(&sealed.ciphertext)
        .bind(&sealed.key_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        // Wake every replica's mint broker so a rotated cred rebuilds its
        // engine (ADR 0057 C2). Best-effort: the value is already committed,
        // so a notify failure must not fail the write.
        let _ = sqlx::query("SELECT pg_notify('org_secret_changed', $1)")
            .bind(&sealed.name)
            .execute(&self.pool)
            .await;
        Ok(engram_core::types::org_secret::OrgSecret {
            name,
            key_id,
            created_at,
            updated_at,
        })
    }

    async fn get_org_secret_sealed(
        &self,
        name: &str,
    ) -> Result<Option<engram_core::types::org_secret::SealedOrgSecret>, MetaError> {
        let row: Option<(Vec<u8>, Vec<u8>, Vec<u8>, String)> = sqlx::query_as(
            "SELECT wrapped_dek, nonce, ciphertext, key_id FROM org_secrets WHERE name = $1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(row.map(|(wrapped_dek, nonce, ciphertext, key_id)| {
            engram_core::types::org_secret::SealedOrgSecret {
                name: name.to_string(),
                wrapped_dek,
                nonce,
                ciphertext,
                key_id,
            }
        }))
    }

    async fn list_org_secrets(
        &self,
    ) -> Result<Vec<engram_core::types::org_secret::OrgSecret>, MetaError> {
        let rows: Vec<(
            String,
            String,
            chrono::DateTime<chrono::Utc>,
            chrono::DateTime<chrono::Utc>,
        )> = sqlx::query_as(
            "SELECT name, key_id, created_at, updated_at FROM org_secrets ORDER BY name ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|(name, key_id, created_at, updated_at)| {
                engram_core::types::org_secret::OrgSecret {
                    name,
                    key_id,
                    created_at,
                    updated_at,
                }
            })
            .collect())
    }

    async fn delete_org_secret(&self, name: &str) -> Result<bool, MetaError> {
        let res = sqlx::query("DELETE FROM org_secrets WHERE name = $1")
            .bind(name)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        let deleted = res.rows_affected() > 0;
        if deleted {
            let _ = sqlx::query("SELECT pg_notify('org_secret_changed', $1)")
                .bind(name)
                .execute(&self.pool)
                .await;
        }
        Ok(deleted)
    }

    async fn apply_missing_sandbox_strikes(
        &self,
        present: &[SessionId],
        missing: &[SessionId],
        grace_ticks: i32,
    ) -> Result<Vec<SessionId>, MetaError> {
        if present.is_empty() && missing.is_empty() {
            return Ok(Vec::new());
        }
        let present_ids: Vec<uuid::Uuid> = present.iter().map(|s| s.as_uuid()).collect();
        let missing_ids: Vec<uuid::Uuid> = missing.iter().map(|s| s.as_uuid()).collect();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        if !present_ids.is_empty() {
            sqlx::query(
                "UPDATE sessions SET missing_strikes = 0
                 WHERE id = ANY($1) AND missing_strikes <> 0",
            )
            .bind(&present_ids)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        let mut flipped: Vec<SessionId> = Vec::new();
        if !missing_ids.is_empty() {
            let rows: Vec<(uuid::Uuid, i32)> = sqlx::query_as(
                "UPDATE sessions SET missing_strikes = missing_strikes + 1
                 WHERE id = ANY($1)
                 RETURNING id, missing_strikes",
            )
            .bind(&missing_ids)
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            let crossed: Vec<uuid::Uuid> = rows
                .into_iter()
                .filter(|(_, s)| *s >= grace_ticks)
                .map(|(id, _)| id)
                .collect();
            if !crossed.is_empty() {
                sqlx::query("UPDATE sessions SET missing_strikes = 0 WHERE id = ANY($1)")
                    .bind(&crossed)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
            }
            flipped = crossed.into_iter().map(SessionId).collect();
        }
        tx.commit().await.map_err(db_err)?;
        Ok(flipped)
    }

    async fn enqueue_session_resume(&self, id: SessionId) -> Result<(), MetaError> {
        // Idle → queued (resume origin). Gated on `status='idle'` so a
        // racing resume that already advanced the row is a clean no-op.
        let n = sqlx::query(
            r#"
            UPDATE sessions
               SET status = 'queued', queued_at = NOW(), queue_origin = 'resume',
                   last_active_at = NOW()
             WHERE id = $1 AND status = 'idle'
            "#,
        )
        .bind(id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n > 0 {
            // Matches `delete_pending_session`'s guard below: only a
            // session that actually landed in `queued` needs the
            // fleet-wide scanner wake — the capacity-freed-between-
            // reserve-and-enqueue race this NOTIFY exists for. The
            // `status='idle'` no-op path (a racing resume that already
            // advanced the row) has nothing new for the scanner to place;
            // waking every replica's scanner into a full sweep for it is
            // pure overhead.
            self.notify_placement_changed("enqueued").await;
        }
        Ok(())
    }

    async fn list_queued_sessions_fifo(
        &self,
    ) -> Result<Vec<engram_core::types::session::QueuedSession>, MetaError> {
        let rows = sqlx::query(
            r#"
            SELECT id, status, host_id, sandbox_id, image_uri, mode,
                   created_at, last_active_at,
                   live_disk_manifest_id, live_disk_manifest_version,
                   selected_skills,
                   COALESCE(mem_budget_mib, 0)::BIGINT AS mem_budget_mib,
                   COALESCE(cpu_budget_vcpus, 0) AS cpu_budget_vcpus,
                   queue_origin, queue_prompt, queued_at
            FROM sessions
            WHERE status = 'queued'
            ORDER BY queued_at ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row::queued_session_from_row).collect()
    }

    async fn place_queued_session(
        &self,
        id: SessionId,
        mem_budget_mib: i64,
        cpu_budget_vcpus: i32,
        candidates: &[HostId],
        affinity_len: usize,
    ) -> Result<Option<HostId>, MetaError> {
        if candidates.is_empty() {
            return Ok(None);
        }
        let cand: Vec<uuid::Uuid> = candidates.iter().map(|h| h.as_uuid()).collect();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        // Same FOR UPDATE serialization + 2D fit as `reserve_placement`.
        let host_rows = sqlx::query(
            r#"
            SELECT id, allocatable_mib, total_vcpus
            FROM hosts
            WHERE id = ANY($1) AND status IN ('ready','draining') AND NOT cordoned
            -- ORDER BY id BEFORE `FOR UPDATE`: every placer (any replica, both
            -- this and `place_queued_session`) locks the overlapping host rows
            -- in the SAME (PK) order, so a burst can't lock {A,B} vs {B,A} and
            -- deadlock. The LockRows executor node sits atop the sort, so rows
            -- are locked in id order. (Load test: `deadlock detected` under
            -- concurrent creates before this.)
            ORDER BY id
            FOR UPDATE
            "#,
        )
        .bind(&cand)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        let mut fit: std::collections::HashMap<uuid::Uuid, HostFit> =
            std::collections::HashMap::with_capacity(host_rows.len());
        for r in &host_rows {
            let hid: uuid::Uuid = sqlx::Row::try_get(r, "id").map_err(db_err)?;
            let alloc_mib: i64 = sqlx::Row::try_get(r, "allocatable_mib").map_err(db_err)?;
            let total_vcpus: i32 = sqlx::Row::try_get(r, "total_vcpus").map_err(db_err)?;
            fit.insert(
                hid,
                HostFit {
                    alloc_mib,
                    reserved_mib: 0,
                    cpu_budget: engram_core::types::host::host_cpu_budget(total_vcpus.max(0) as u32),
                    reserved_vcpus: 0,
                },
            );
        }
        let res_rows = sqlx::query(
            r#"
            SELECT host_id,
                   COALESCE(SUM(mem_budget_mib), 0)::BIGINT AS reserved_mib,
                   COALESCE(SUM(cpu_budget_vcpus), 0)::BIGINT AS reserved_vcpus
            FROM sessions
            WHERE host_id = ANY($1)
              AND status IN ('pending','created','active',
                             'evacuating','evicting')
              AND (status <> 'pending' OR last_active_at > NOW() - INTERVAL '10 minutes')
            GROUP BY host_id
            "#,
        )
        .bind(&cand)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        for r in &res_rows {
            let h: uuid::Uuid = sqlx::Row::try_get(r, "host_id").map_err(db_err)?;
            let mem: i64 = sqlx::Row::try_get(r, "reserved_mib").map_err(db_err)?;
            let cpu: i64 = sqlx::Row::try_get(r, "reserved_vcpus").map_err(db_err)?;
            if let Some(f) = fit.get_mut(&h) {
                f.reserved_mib = mem;
                f.reserved_vcpus = cpu;
            }
        }
        let Some(picked) = choose_placement_host(
            &cand,
            affinity_len,
            &fit,
            mem_budget_mib,
            cpu_budget_vcpus as i64,
        ) else {
            tx.rollback().await.map_err(db_err)?;
            return Ok(None);
        };
        // Flip queued → pending on the picked host. 0 rows = lost a race
        // (already left `queued`); roll back, report no placement.
        let n = sqlx::query(
            r#"
            UPDATE sessions
               SET status = 'pending', host_id = $2, last_active_at = NOW()
             WHERE id = $1 AND status = 'queued'
            "#,
        )
        .bind(id.as_uuid())
        .bind(picked)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n == 0 {
            tx.rollback().await.map_err(db_err)?;
            return Ok(None);
        }
        tx.commit().await.map_err(db_err)?;
        Ok(Some(HostId(picked)))
    }

    async fn requeue_session(&self, id: SessionId) -> Result<bool, MetaError> {
        let n = sqlx::query(
            r#"
            UPDATE sessions
               SET status = 'queued', host_id = NULL, last_active_at = NOW()
             WHERE id = $1 AND status = 'pending'
            "#,
        )
        .bind(id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(db_err)?
        .rows_affected();
        Ok(n > 0)
    }

    async fn requeue_stale_pending(
        &self,
        older_than: std::time::Duration,
    ) -> Result<u64, MetaError> {
        let n = sqlx::query(
            r#"
            UPDATE sessions
               SET status = 'queued', host_id = NULL, last_active_at = NOW()
             WHERE status = 'pending'
               AND queue_origin IS NOT NULL
               AND last_active_at < NOW() - make_interval(secs => $1::bigint)
            "#,
        )
        .bind(older_than.as_secs() as i64)
        .execute(&self.pool)
        .await
        .map_err(db_err)?
        .rows_affected();
        Ok(n)
    }

    async fn queued_demand(&self) -> Result<engram_core::types::session::QueuedDemand, MetaError> {
        let row: (i64, i64, i64) = sqlx::query_as(
            r#"
            SELECT COUNT(*)::BIGINT,
                   COALESCE(SUM(mem_budget_mib), 0)::BIGINT,
                   COALESCE(SUM(cpu_budget_vcpus), 0)::BIGINT
            FROM sessions WHERE status = 'queued'
            "#,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(engram_core::types::session::QueuedDemand {
            sessions: row.0.max(0) as u64,
            mem_mib: row.1.max(0) as u64,
            vcpus: row.2.max(0) as u64,
        })
    }

    async fn per_host_reserved(
        &self,
    ) -> Result<
        std::collections::HashMap<HostId, engram_core::types::host::ReservedBudget>,
        MetaError,
    > {
        // Same predicate as `reserve_placement` / `fleet_free_mib` — the
        // memory-reserving states, with crash-orphaned `pending` rows
        // excluded — but summing BOTH budget dimensions (ADR 0048).
        let rows: Vec<(uuid::Uuid, i64, i64)> = sqlx::query_as(
            r#"
            SELECT host_id,
                   COALESCE(SUM(mem_budget_mib), 0)::BIGINT,
                   COALESCE(SUM(cpu_budget_vcpus), 0)::BIGINT
            FROM sessions
            WHERE host_id IS NOT NULL
              AND status IN ('pending','created','active',
                             'evacuating','evicting')
              -- ADR 0048: gate on last_active_at, not created_at — a session
              -- can sit `queued` for many minutes before `place_queued_session`
              -- flips it to `pending` (bumping last_active_at), and an old
              -- created_at would make that fresh reservation look crash-orphaned
              -- and leak (overcommit). reserve_placement sets both to NOW().
              AND (status <> 'pending' OR last_active_at > NOW() - INTERVAL '10 minutes')
            GROUP BY host_id
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|(h, mem_mib, vcpus)| {
                (
                    HostId(h),
                    engram_core::types::host::ReservedBudget { mem_mib, vcpus },
                )
            })
            .collect())
    }

    async fn delete_pending_session(&self, session_id: SessionId) -> Result<(), MetaError> {
        let n = sqlx::query(
            "DELETE FROM sessions WHERE id = $1 AND status = 'pending' AND sandbox_id IS NULL",
        )
        .bind(session_id.as_uuid())
        .execute(&self.pool)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n > 0 {
            // A reservation was released — the freed budget may now fit a
            // queued session.
            self.notify_placement_changed("pending_deleted").await;
        }
        Ok(())
    }

    async fn get_session(&self, id: SessionId) -> Result<Session, MetaError> {
        let row = sqlx::query(
            r#"
            SELECT id, status, host_id, sandbox_id,
                   image_uri, mode,
                   created_at, last_active_at,
                   live_disk_manifest_id, live_disk_manifest_version,
                   selected_skills
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
                  AND status IN ('pending','created','active',
                                 'evacuating','evicting')
                  -- ADR 0048: gate on last_active_at, not created_at — a session
              -- can sit `queued` for many minutes before `place_queued_session`
              -- flips it to `pending` (bumping last_active_at), and an old
              -- created_at would make that fresh reservation look crash-orphaned
              -- and leak (overcommit). reserve_placement sets both to NOW().
              AND (status <> 'pending' OR last_active_at > NOW() - INTERVAL '10 minutes')
                GROUP BY host_id
            ) r ON r.host_id = h.id
            WHERE h.status IN ('ready','draining') AND NOT h.cordoned
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
        // `evicting` matters most: it keeps `sandbox_id` BOUND while the
        // pipeline runs, so the eviction scanner re-picks a mid-eviction
        // session after a coord roll and its "sandbox no longer bound"
        // guard (which reads `sessions.sandbox_id` directly — ADR 0047,
        // no in-memory registry) still sees the live binding. When
        // `evicting` was missing, a roll mid-eviction dropped the session
        // from the active set, the scanner never re-picked it, and the
        // budget exhausted into a spurious HostLost with the VM still
        // running (prod session 5cfb90b8, 2026-06-03).
        let rows = sqlx::query(
            r#"
            SELECT id, status, host_id, sandbox_id,
                   image_uri, mode,
                   created_at, last_active_at,
                   live_disk_manifest_id, live_disk_manifest_version,
                   selected_skills
            FROM sessions
            WHERE status IN ('pending','created','active',
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

    async fn list_active_assignments_with_budgets_on_host(
        &self,
        host_id: HostId,
    ) -> Result<Vec<engram_core::types::session::SandboxAssignment>, MetaError> {
        let rows: Vec<(Uuid, Uuid, i64, i32)> = sqlx::query_as(
            r#"
            SELECT id, sandbox_id,
                   COALESCE(mem_budget_mib, 0)::BIGINT,
                   COALESCE(cpu_budget_vcpus, 0)
            FROM sessions
            WHERE host_id = $1 AND status = 'active' AND sandbox_id IS NOT NULL
            "#,
        )
        .bind(host_id.as_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(
                |(s, sb, mem, cpu)| engram_core::types::session::SandboxAssignment {
                    session_id: SessionId::from(s),
                    sandbox_id: SandboxId::from(sb),
                    mem_budget_mib: mem,
                    cpu_budget_vcpus: cpu,
                },
            )
            .collect())
    }

    async fn delete_host(
        &self,
        id: HostId,
    ) -> Result<engram_core::types::session::DeleteHostOutcome, MetaError> {
        use engram_core::types::session::DeleteHostOutcome;
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        // Refuse while any session is still bound — deleting the row out
        // from under a live session would orphan its routing.
        let bound: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)::BIGINT FROM sessions
            WHERE host_id = $1
              AND status IN ('pending','created','active',
                             'evacuating','evicting')
            "#,
        )
        .bind(id.as_uuid())
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        if bound > 0 {
            tx.rollback().await.map_err(db_err)?;
            return Ok(DeleteHostOutcome::SessionsBound(bound as u64));
        }
        // Detach terminal/idle stragglers (defensive against an FK), then
        // delete. 0 rows deleted = already gone → idempotent Deleted.
        sqlx::query("UPDATE sessions SET host_id = NULL WHERE host_id = $1")
            .bind(id.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        sqlx::query("DELETE FROM hosts WHERE id = $1")
            .bind(id.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(DeleteHostOutcome::Deleted)
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
        // ADR 0048 (queue fairness): a session leaving a memory-reserving
        // state (e.g. Active → Idle) frees its budget — wake the queue
        // scanner so a waiting session doesn't sit for the poll fallback.
        // Fired after commit (the freed capacity is only real once
        // committed); the reverse direction (entering a reserving state)
        // never frees anything, so it's not a wake trigger.
        if current.reserves_host_memory() && !target.reserves_host_memory() {
            self.notify_placement_changed("session_freed").await;
        }
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

    async fn list_active_sessions_desynced(
        &self,
        stuck_for_secs: i64,
    ) -> Result<Vec<engram_core::traits::metadata::DesyncedSession>, MetaError> {
        // Track A: two desync signatures the idle backstop (silence-only)
        // misses. Per Active+bound session, find the latest non-rewound
        // event overall and the latest run-lifecycle event, then classify:
        //   - stuck_open_run:    latest event IS a `run_started` (the run
        //                        opened and emitted nothing since).
        //   - orphan_after_close: latest event is a run-scoped event but
        //                        the most recent run-lifecycle event is NOT
        //                        a `run_started` — i.e. an event landed
        //                        after the run closed, with no open run
        //                        (the `bf3dbbcb` shape).
        // A healthy in-progress run is excluded: its latest run-lifecycle
        // event is `run_started`, so an `agent_message`/`tool_call_*` tail
        // pairs to an OPEN run and is not orphaned. The `last_event_at`
        // floor ensures we only fire once the session has been wedged for
        // the TTL (a live run keeps bumping last_event_at).
        let rows = sqlx::query(
            r#"
            WITH active AS (
                SELECT id, sandbox_id, COALESCE(last_event_at, created_at) AS le
                  FROM sessions
                 WHERE status = 'active' AND sandbox_id IS NOT NULL
            ),
            latest AS (
                SELECT DISTINCT ON (e.session_id) e.session_id, e.kind
                  FROM session_events e
                  JOIN active a ON a.id = e.session_id
                 WHERE e.rewound_at IS NULL
                 ORDER BY e.session_id, e.idx DESC
            ),
            latest_run AS (
                SELECT DISTINCT ON (e.session_id) e.session_id, e.kind
                  FROM session_events e
                  JOIN active a ON a.id = e.session_id
                 WHERE e.rewound_at IS NULL
                   AND e.kind IN ('run_started', 'run_completed', 'run_interrupted')
                 ORDER BY e.session_id, e.idx DESC
            )
            SELECT a.id, a.sandbox_id, a.le AS last_event_at, l.kind AS latest_kind,
                   CASE WHEN l.kind = 'run_started'
                        THEN 'stuck_open_run'
                        ELSE 'orphan_after_close'
                   END AS signature
              FROM active a
              JOIN latest l ON l.session_id = a.id
              LEFT JOIN latest_run lr ON lr.session_id = a.id
             WHERE a.le < NOW() - ($1::bigint * INTERVAL '1 second')
               AND (
                     l.kind = 'run_started'
                  OR (l.kind IN ('agent_message', 'tool_call_started', 'tool_call_completed')
                      AND COALESCE(lr.kind, '') <> 'run_started')
                   )
            "#,
        )
        .bind(stuck_for_secs)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let id: uuid::Uuid = r
                .try_get("id")
                .map_err(|e| MetaError::Serialization(format!("desync id: {e}")))?;
            let sandbox: uuid::Uuid = r
                .try_get("sandbox_id")
                .map_err(|e| MetaError::Serialization(format!("desync sandbox_id: {e}")))?;
            let latest_kind: String = r
                .try_get("latest_kind")
                .map_err(|e| MetaError::Serialization(format!("desync latest_kind: {e}")))?;
            let signature: String = r
                .try_get("signature")
                .map_err(|e| MetaError::Serialization(format!("desync signature: {e}")))?;
            let last_event_at: chrono::DateTime<chrono::Utc> = r
                .try_get("last_event_at")
                .map_err(|e| MetaError::Serialization(format!("desync last_event_at: {e}")))?;
            out.push(engram_core::traits::metadata::DesyncedSession {
                session_id: SessionId::from(id),
                sandbox_id: SandboxId::from(sandbox),
                latest_kind,
                signature,
                last_event_at,
            });
        }
        Ok(out)
    }

    async fn rebind_session(
        &self,
        id: SessionId,
        host_id: HostId,
        sandbox_id: SandboxId,
    ) -> Result<(), MetaError> {
        // ADR 0045 C2: ONE UPDATE — the ownership oracle
        // (`session.sandbox_id == sandbox`) flips atomically with the
        // host rebind (the post-copy `Committing` persist).
        //
        // Issue #215: reset `missing_strikes = 0` — this re-keys the
        // session onto a fresh sandbox (and host), so any in-flight
        // reconcile strike streak against the old sandbox is no longer
        // consecutive and must not bleed into the new binding's grace.
        let n = sqlx::query(
            r#"
            UPDATE sessions SET host_id = $2, sandbox_id = $3, missing_strikes = 0, updated_at = NOW() WHERE id = $1
            "#,
        )
        .bind(id.as_uuid())
        .bind(host_id.as_uuid())
        .bind(sandbox_id.as_uuid())
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
        //
        // Issue #215: BOTH branches reset `missing_strikes = 0`. The
        // reconcile strike counter (`apply_missing_sandbox_strikes`)
        // means "N CONSECUTIVE heartbeats in which THIS session's
        // bound sandbox was missing from its host's running set". A
        // rebind points the row at a brand-new sandbox and an unbind
        // detaches it entirely — either way the previous streak is no
        // longer consecutive against the current binding, so carrying
        // it forward would collapse the 3-tick grace for a freshly
        // resumed/migrated session (one transient under-report on the
        // new host is strike 3, dismantling a healthy VM). The strike
        // column is keyed by session id alone, so re-keying the
        // sandbox must explicitly clear it here.
        let n = if sandbox_id.is_some() {
            sqlx::query(
                "UPDATE sessions SET sandbox_id = $2, missing_strikes = 0, updated_at = NOW() \
                 WHERE id = $1",
            )
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
                        missing_strikes            = 0,
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

    // ---- Issue #211: guarded CAS overrides ----
    //
    // The three writers above are blind `WHERE id = $1` UPDATEs. These
    // overrides condition the same write on the row's current
    // `sandbox_id` and `status`, so a racing actor can't bind a live
    // sandbox onto a row that concurrently went terminal (defeating the
    // orphan reap), and reconcile can't null a freshly-landed rebind.
    //
    // `0 rows` is disambiguated into `Conflict` (row exists but the guard
    // rejected it) vs `NotFound` (no such id) with a cheap follow-up
    // existence probe — the same shape `transition_session` uses.

    async fn assign_session_sandbox_guarded(
        &self,
        id: SessionId,
        sandbox_id: Option<SandboxId>,
        expected_current: Option<Option<SandboxId>>,
        allowed_states: &[SessionState],
    ) -> Result<(), MetaError> {
        // The `None` clear path additionally tears down the live disk
        // manifest + bumps chunk_generation; reuse the existing blind
        // setter inside a guarded TX rather than duplicating that logic.
        let states: Vec<String> = allowed_states
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();
        let expected_uuid = expected_current.map(|o| o.map(|s| s.as_uuid()));

        let mut tx = self.pool.begin().await.map_err(db_err)?;
        // Lock + verify the row under the guard. SELECT ... FOR UPDATE so
        // a concurrent terminate/transition serializes behind us.
        let row: Option<(Option<uuid::Uuid>, String)> =
            sqlx::query_as("SELECT sandbox_id, status FROM sessions WHERE id = $1 FOR UPDATE")
                .bind(id.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
        let Some((cur_sandbox, status)) = row else {
            tx.rollback().await.map_err(db_err)?;
            return Err(MetaError::NotFound);
        };
        if let Some(expected) = expected_uuid {
            if cur_sandbox != expected {
                tx.rollback().await.map_err(db_err)?;
                return Err(MetaError::Conflict(format!(
                    "assign_session_sandbox CAS: sandbox_id is {cur_sandbox:?}, expected {expected:?}"
                )));
            }
        }
        if !states.is_empty() && !states.contains(&status) {
            tx.rollback().await.map_err(db_err)?;
            return Err(MetaError::Conflict(format!(
                "assign_session_sandbox CAS: status is {status}, not in {states:?}"
            )));
        }
        // Issue #215: clear `missing_strikes` on both bind and unbind —
        // see the comment in `assign_session_sandbox`. A re-key / unbind
        // breaks the reconcile strike streak's consecutiveness.
        if sandbox_id.is_some() {
            sqlx::query(
                "UPDATE sessions SET sandbox_id = $2, missing_strikes = 0, updated_at = NOW() \
                 WHERE id = $1",
            )
            .bind(id.as_uuid())
            .bind(sandbox_id.map(|s| s.as_uuid()))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        } else {
            let n = sqlx::query(
                "UPDATE sessions
                    SET sandbox_id                 = NULL,
                        missing_strikes            = 0,
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
            if n > 0 {
                sqlx::query(
                    "UPDATE chunk_generation SET generation = generation + 1 WHERE id = TRUE",
                )
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            }
        }
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn assign_session_host_guarded(
        &self,
        id: SessionId,
        host_id: Option<HostId>,
        expected_current: Option<Option<SandboxId>>,
        allowed_states: &[SessionState],
    ) -> Result<(), MetaError> {
        let states: Vec<String> = allowed_states
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();
        let expected_uuid = expected_current.map(|o| o.map(|s| s.as_uuid()));
        let n = sqlx::query(
            r#"
            UPDATE sessions SET host_id = $2, updated_at = NOW()
            WHERE id = $1
              AND ($3::text[] IS NULL OR status = ANY($3))
              AND ($4::boolean IS FALSE OR sandbox_id IS NOT DISTINCT FROM $5)
            "#,
        )
        .bind(id.as_uuid())
        .bind(host_id.map(|h| h.as_uuid()))
        .bind(if states.is_empty() {
            None
        } else {
            Some(states)
        })
        .bind(expected_uuid.is_some())
        .bind(expected_uuid.flatten())
        .execute(&self.pool)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n == 0 {
            return Err(self.conflict_or_not_found(id).await);
        }
        Ok(())
    }

    async fn rebind_session_guarded(
        &self,
        id: SessionId,
        host_id: HostId,
        sandbox_id: SandboxId,
        expected_current: Option<Option<SandboxId>>,
        allowed_states: &[SessionState],
    ) -> Result<(), MetaError> {
        let states: Vec<String> = allowed_states
            .iter()
            .map(|s| s.as_str().to_string())
            .collect();
        let expected_uuid = expected_current.map(|o| o.map(|s| s.as_uuid()));
        // Issue #215: re-keying onto a fresh sandbox clears the stale
        // reconcile strike streak (see `assign_session_sandbox`).
        let n = sqlx::query(
            r#"
            UPDATE sessions SET host_id = $2, sandbox_id = $3, missing_strikes = 0, updated_at = NOW()
            WHERE id = $1
              AND ($4::text[] IS NULL OR status = ANY($4))
              AND ($5::boolean IS FALSE OR sandbox_id IS NOT DISTINCT FROM $6)
            "#,
        )
        .bind(id.as_uuid())
        .bind(host_id.as_uuid())
        .bind(sandbox_id.as_uuid())
        .bind(if states.is_empty() {
            None
        } else {
            Some(states)
        })
        .bind(expected_uuid.is_some())
        .bind(expected_uuid.flatten())
        .execute(&self.pool)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n == 0 {
            return Err(self.conflict_or_not_found(id).await);
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
        // ADR 0068: persist the register-time capability vector too — a
        // host's very first row (before its first heartbeat) should
        // already carry whatever `probe_all` measured at startup, not
        // sit at `'{}'::jsonb` (schema 0) until 5s later.
        let capabilities = serde_json::to_value(&host.capabilities)
            .map_err(|e| MetaError::Serialization(e.to_string()))?;
        sqlx::query(
            r#"
            INSERT INTO hosts (id, hostname, cloud_metadata,
                               capacity_total_gb, capacity_used_gb,
                               capacity_total_mib, capacity_used_mib,
                               running_sandboxes_count,
                               last_heartbeat_at, status, host_addr,
                               capabilities, created_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, NOW())
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
                capabilities            = EXCLUDED.capabilities,
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
        .bind(capabilities)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        // New or re-registered host → schedulable. Always fires (both the
        // insert and the re-register arm land here); harmless if nothing
        // was waiting.
        self.notify_placement_changed("host_upserted").await;
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
                   util_base_shm_mib, util_parked_pss_mib, util_running_pss_mib,
                   ready_images, local_snapshots, current_bundles,
                   cordoned, total_vcpus, wire_version, stages_images, capabilities,
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
        hb: engram_core::types::host::HostHeartbeat,
    ) -> Result<(), MetaError> {
        // ADR 0047: the single per-heartbeat UPDATE — capacity +
        // utilization + the scheduling state every replica reads
        // (ready_images / local_snapshots / current_bundles /
        // total_vcpus). `cordoned` is deliberately absent: it is
        // coordinator-owned and only `set_host_cordoned` writes it.
        let ready_images = serde_json::to_value(&hb.ready_images)
            .map_err(|e| MetaError::Serialization(e.to_string()))?;
        let local_snapshots = serde_json::to_value(&hb.local_snapshots)
            .map_err(|e| MetaError::Serialization(e.to_string()))?;
        let current_bundles = serde_json::to_value(&hb.current_bundles)
            .map_err(|e| MetaError::Serialization(e.to_string()))?;
        // ADR 0068: this tick's re-probed capability vector.
        let capabilities = serde_json::to_value(&hb.capabilities)
            .map_err(|e| MetaError::Serialization(e.to_string()))?;
        let n = sqlx::query(
            // Issue #230: `dead` is terminal w.r.t. heartbeats — see
            // `HostStatus::can_transition_to`. The dead-host sweep
            // (`mark_host_dead_and_orphan_sessions`) marks a partitioned
            // host `dead` and UNBINDS its sessions (host_id/sandbox_id
            // NULLed). Without this guard the host's very next heartbeat
            // blindly wrote `status = $2` (ready/draining from the agent's
            // self-report), resurrecting the row to schedulable while its
            // former sessions sit unbound — a zombie host taking new
            // placements. The CASE pins `dead` so a `dead` row returns to
            // `ready` ONLY via an explicit re-register (`upsert_host`,
            // which conflicts on `(id)` and sets `status = EXCLUDED.status`
            // = ready). `ready`↔`draining` stay heartbeat-overridable:
            // `draining` is the agent's own preStop flag and a host that
            // finished/aborted its drain legitimately reports ready again.
            r#"UPDATE hosts
                  SET status = CASE WHEN status = 'dead' THEN 'dead' ELSE $2 END,
                      capacity_total_mib = $3,
                      capacity_used_mib = $4,
                      running_sandboxes_count = $5,
                      util_disk_total_mib = $6,
                      util_disk_used_mib = $7,
                      util_mem_total_mib = $8,
                      util_mem_used_mib = $9,
                      util_cpu_pct = $10,
                      allocatable_mib = $11,
                      ready_images = $12,
                      local_snapshots = $13,
                      current_bundles = $14,
                      total_vcpus = $15,
                      wire_version = $16,
                      util_base_shm_mib = $17,
                      util_parked_pss_mib = $18,
                      util_running_pss_mib = $19,
                      capabilities = $20,
                      stages_images = $21,
                      last_heartbeat_at = NOW(),
                      updated_at = NOW()
                WHERE id = $1"#,
        )
        .bind(id.as_uuid())
        .bind(hb.status.as_str())
        .bind(hb.capacity.total_mib as i64)
        .bind(hb.capacity.used_mib as i64)
        .bind(hb.capacity.running_sandboxes as i32)
        .bind(hb.utilization.disk_total_mib as i64)
        .bind(hb.utilization.disk_used_mib as i64)
        .bind(hb.utilization.mem_total_mib as i64)
        .bind(hb.utilization.mem_used_mib as i64)
        .bind(hb.utilization.cpu_pct)
        .bind(hb.utilization.allocatable_mib as i64)
        .bind(ready_images)
        .bind(local_snapshots)
        .bind(current_bundles)
        .bind(hb.total_vcpus as i32)
        .bind(hb.wire_version as i32)
        // Issue #540: base_shm_pending_mib has no column (transient,
        // already folded into allocatable_mib above) — not bound here.
        .bind(hb.utilization.base_shm_mib as i64)
        .bind(hb.utilization.parked_pss_mib as i64)
        .bind(hb.utilization.running_pss_mib as i64)
        .bind(capabilities)
        .bind(hb.stages_images)
        .execute(&self.pool)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n == 0 {
            return Err(MetaError::NotFound);
        }
        Ok(())
    }

    async fn set_host_cordoned(&self, id: HostId, cordoned: bool) -> Result<(), MetaError> {
        // ADR 0047: the coordinator-owned cordon bit. Heartbeats never
        // write this column, so the flip sticks until explicit uncordon.
        let n = sqlx::query(r#"UPDATE hosts SET cordoned = $2, updated_at = NOW() WHERE id = $1"#)
            .bind(id.as_uuid())
            .bind(cordoned)
            .execute(&self.pool)
            .await
            .map_err(db_err)?
            .rows_affected();
        if n == 0 {
            return Err(MetaError::NotFound);
        }
        if !cordoned {
            // Uncordoning makes the host schedulable again.
            self.notify_placement_changed("host_uncordoned").await;
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
                   util_base_shm_mib, util_parked_pss_mib, util_running_pss_mib,
                   ready_images, local_snapshots, current_bundles,
                   cordoned, total_vcpus, wire_version, stages_images, capabilities,
                   last_heartbeat_at, status, host_addr
              FROM hosts
             -- Only `ready` hosts are strike-out candidates. A `draining`
             -- host is host-reported operator territory (agent shutdown /
             -- preStop), and a `cordoned` host is coordinator territory:
             -- mid image-roll (where ADR 0044 K2 reattach keeps its VMs
             -- alive across the brief pod-swap heartbeat gap) or mid
             -- scale-down drain (ADR 0048). The detector must not race a
             -- roll and route the reattaching sessions to Idle out from
             -- under the successor — BUT a cordon must not shield a
             -- genuinely-dead host forever (a wave victim that dies
             -- mid-drain still needs its sessions rehomed), so cordoned
             -- hosts are struck out at a 10× stale threshold (ADR 0047).
             WHERE status = 'ready'
               AND (
                     (NOT cordoned
                      AND last_heartbeat_at < NOW() - make_interval(secs => $1::bigint))
                  OR last_heartbeat_at < NOW() - make_interval(secs => $1::bigint * 10)
               )
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

    async fn record_snapshot(&self, snap: SnapshotRecord) -> Result<bool, MetaError> {
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
        // Issue #529: `RETURNING (xmax = 0)` tells the caller whether this
        // call INSERTed a fresh row or UPDATEd an existing one — Postgres's
        // standard idiom for "was this an insert". The heartbeat reconcile
        // uses it to emit `SnapshotTaken` exactly once, on the row's first
        // landing, regardless of which coord (if any) survived the
        // original capture.
        let row = sqlx::query(
            r#"
            INSERT INTO snapshots
                (id, session_id, host_id,
                 image_version, size_bytes, created_at, last_accessed_at,
                 disk_manifest_id, disk_manifest_version,
                 memory_manifest_id, memory_manifest_version,
                 recoverable, aux_bundles, events_cursor, fc_snapshot_version)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
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
                -- ADR 0068: same idempotency guard — a re-record (e.g.
                -- the checkpoint-advert reconcile re-ingesting a row the
                -- eviction pipeline already stamped) must not blank out
                -- an already-known capture-time FC snapshot version.
                fc_snapshot_version     = COALESCE(EXCLUDED.fc_snapshot_version, snapshots.fc_snapshot_version),
                updated_at              = NOW()
            RETURNING (xmax = 0) AS inserted
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
        .bind(&snap.fc_snapshot_version)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let inserted: bool = sqlx::Row::try_get(&row, "inserted").map_err(db_err)?;
        sqlx::query("UPDATE chunk_generation SET generation = generation + 1 WHERE id = TRUE")
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(inserted)
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

    async fn prune_orphan_base_snapshots(
        &self,
        grace: chrono::Duration,
    ) -> Result<Vec<engram_core::types::SnapshotId>, MetaError> {
        // The `session_id IS NULL` mirror of `prune_session_snapshots`.
        // A base/template snapshot is orphaned once no `enabled_images`
        // row points at it via `base_snapshot_id`; an image refresh
        // swaps that pointer to a fresh capture (`upsert_enabled_image`'s
        // ON CONFLICT) and leaves the prior row dangling. Delete those
        // past `grace`, bump `chunk_generation` in the same TX (GC-barrier
        // symmetry), and let the chunk-GC + snapshot-blob-GC sweeps reclaim
        // the freed chunks / portable `snapshots/<id>/` blobs.
        //
        // The subquery's `base_snapshot_id IS NOT NULL` keeps a stray NULL
        // out of the `NOT IN` set — a NULL there makes `NOT IN` match zero
        // rows, which would silently disable the reaper. It is deliberately
        // unfiltered by `soft_deleted_at`: a soft-deleted image's base is
        // still chunk-lineage-pinned (ADR 0021 P1.8), so it must stay. The
        // `base_snapshot_id` FK (no `ON DELETE`) is a hard backstop against
        // ever deleting an in-use base even if this predicate regressed.
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let rows: Vec<(uuid::Uuid,)> = sqlx::query_as(
            r#"
            DELETE FROM snapshots s
            WHERE s.session_id IS NULL
              AND s.created_at < NOW() - $1::interval
              AND s.id NOT IN (
                  SELECT base_snapshot_id
                  FROM enabled_images
                  WHERE base_snapshot_id IS NOT NULL
              )
            RETURNING s.id
            "#,
        )
        .bind(sqlx::postgres::types::PgInterval {
            months: 0,
            days: 0,
            microseconds: grace.num_microseconds().unwrap_or(i64::MAX),
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
                   recoverable, aux_bundles, events_cursor, fc_snapshot_version
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
                   recoverable, aux_bundles, events_cursor, fc_snapshot_version
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
                   recoverable, aux_bundles, events_cursor, fc_snapshot_version
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
                       updated_at = NOW(),
                       -- Track A: honest activity clock, bumped on every
                       -- event append (unlike last_active_at, which only
                       -- moves on state transitions).
                       last_event_at = NOW()
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

    async fn notify_session_delta(
        &self,
        session_id: SessionId,
        payload: &serde_json::Value,
    ) -> Result<(), MetaError> {
        // Phase 1c: fan one EPHEMERAL token chunk out cross-replica. Unlike
        // `append_session_event`, there is NO row and NO idx — the full
        // event rides inline so every replica's `PgListener` re-broadcasts
        // it to its local SSE bus without a fetch. A bare `pg_notify`
        // commits immediately (its own autocommit unit).
        let body =
            serde_json::json!({ "session_id": session_id.as_uuid().to_string(), "event": payload })
                .to_string();
        // Postgres caps a NOTIFY payload at 8000 bytes. The harness clamps
        // chunks (MAX_CHUNK_BYTES = 6 KiB) so the envelope fits with margin;
        // guard defensively anyway — an oversize chunk is simply not
        // streamed, and the durable terminal `agent_message` still carries
        // the full text, so the transcript is never wrong.
        if body.len() > 7800 {
            tracing::debug!(
                session_id = %session_id,
                len = body.len(),
                "session delta exceeds NOTIFY payload limit; skipping live stream",
            );
            return Ok(());
        }
        sqlx::query("SELECT pg_notify('session_event_deltas', $1)")
            .bind(body)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
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

    async fn prompt_received_seconds_ago(
        &self,
        session_id: SessionId,
        prompt_id: &str,
    ) -> Result<Option<f64>, MetaError> {
        // Issue #527 Phase 1: the receipt row is coordinator-authoritative
        // and excluded from the rewind tombstone (see
        // `rewind_session_to_cursor` below), so it is always the live head
        // for this `prompt_id` — DESC LIMIT 1 is defensive against the
        // retryable-Conflict duplicate-receipt case (a client retry of a
        // rejected SendPrompt reusing the same `prompt_id` — see PR #556
        // review finding #3) rather than a happy-path guarantee.
        //
        // PR #556 review finding #1: `NOW() - created_at` is computed here,
        // PG-side, in the same query as the row read — a single clock, so
        // there's no coordinator-vs-Postgres (or cross-replica) skew to
        // bias or drop samples.
        let row = sqlx::query(
            r#"
            SELECT EXTRACT(EPOCH FROM (NOW() - created_at))::float8 AS secs_ago
              FROM session_events
             WHERE session_id = $1 AND kind = 'prompt_received' AND payload->>'prompt_id' = $2
             ORDER BY idx DESC
             LIMIT 1
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(prompt_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| sqlx::Row::try_get::<f64, _>(&r, "secs_ago").map_err(db_err))
            .transpose()
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
               AND (kind = 'file_shared'
                    OR (kind = 'integration_asset' AND payload->>'surface' = 'asset'))
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
                    // ADR 0056: a durable integration asset (surface='asset')
                    // survives the rewind. Build the line generically from the
                    // payload — prefer a fetchable URL, then data.url/title,
                    // else fall back to the provider/asset_kind pair.
                    "integration_asset" => {
                        let provider = payload
                            .get("provider")
                            .and_then(|v| v.as_str())
                            .unwrap_or("integration");
                        let asset_kind = payload
                            .get("asset_kind")
                            .and_then(|v| v.as_str())
                            .unwrap_or("asset");
                        let detail = payload
                            .get("fetchable")
                            .and_then(|f| f.get("url"))
                            .and_then(|v| v.as_str())
                            .or_else(|| {
                                payload
                                    .get("data")
                                    .and_then(|d| d.get("url").or_else(|| d.get("title")))
                                    .and_then(|v| v.as_str())
                            });
                        match detail {
                            Some(d) => {
                                format!(
                                    "A {provider} {asset_kind} was produced and still exists: {d}"
                                )
                            }
                            None => {
                                format!("A {provider} {asset_kind} was produced and still exists")
                            }
                        }
                    }
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
        // Issue #529: exclude coordinator-fact kinds — `status_changed`,
        // `snapshot_taken`, `evicted`, `resumed`, `recovered_from_checkpoint`.
        // Those are control-plane bookkeeping the coordinator itself
        // appended around the eviction/resume boundary; they stay true
        // regardless of what the guest remembers, so rewinding them was
        // what made every clean evict→resume "roll back" (median 4
        // events) even with nothing lost. Everything guest-derived
        // (run_*, agent_message*, tool_call_*, exec_*, stdout/stderr,
        // prompt_*, harness_idle, user_question, question_answered,
        // file_changed, file_shared, integration_asset, …) still rewinds.
        //
        // Issue #527 Phase 1: `prompt_received` is ALSO excluded here — it
        // is a coordinator-authoritative fact ("the user asked at time T")
        // that stays true across a guest-state rewind (the resume rewinds
        // the HARNESS's view of the world, not whether the user sent the
        // prompt). Without this exclusion, every resume-with-rollback would
        // tombstone the receipt row and inflate `rolled_back` by one,
        // masking the real signal this issue exists to measure.
        //
        // Per issue #527's Guardrails merge-coordination note: the two
        // sibling exclusion lists compose into one `AND kind NOT IN (...)`
        // predicate rather than stacking separate `AND kind <>` clauses.
        let tombstoned = sqlx::query(
            r#"
            UPDATE session_events
               SET rewound_at = NOW()
             WHERE session_id = $1 AND idx > $2 AND rewound_at IS NULL
               AND kind NOT IN (
                   'status_changed', 'snapshot_taken', 'evicted',
                   'resumed', 'recovered_from_checkpoint', 'prompt_received'
               )
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(events_cursor)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();

        if tombstoned == 0 {
            // Checkpoint was already the head — no rewind — OR (PR #556
            // review finding #5) the only post-cursor rows are excluded
            // `prompt_received` receipts (see the exclusion above): nothing
            // user-visible actually rewound either way, so this stays the
            // correct no-op branch. Don't bump the epoch (keeps the no-op
            // clean); caller emits nothing.
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
                 last_refreshed_at, created_at, updated_at, soft_deleted_at, capture_env)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, NULL, NULL, $14)
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
                capture_env           = EXCLUDED.capture_env,
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
        .bind(sqlx::types::Json(&image.capture_env))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query("UPDATE chunk_generation SET generation = generation + 1 WHERE id = TRUE")
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        // Issue #535 (a): wake every coordinator replica's boot-bundle cache so
        // a re-bake/re-enable is visible on the next create without waiting
        // out the cache's TTL. NOTIFY is transactional — issuing it here (vs.
        // after commit on a separate connection, like org_secret_changed) means
        // it's delivered iff this transaction actually commits, and no
        // separate best-effort round trip is needed.
        sqlx::query("SELECT pg_notify('enabled_image_changed', $1)")
            .bind(&image.image_uri)
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
                   last_refreshed_at, created_at, updated_at, soft_deleted_at,
                   capture_env
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
                   last_refreshed_at, created_at, updated_at, soft_deleted_at,
                   capture_env
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
                   last_refreshed_at, created_at, updated_at, soft_deleted_at,
                   capture_env
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
        // Issue #535 (a): see the identical NOTIFY in `upsert_enabled_image` —
        // a soft-delete also has to invalidate a cached bundle so create-time
        // strictness (rejecting a disabled image) takes effect immediately.
        sqlx::query("SELECT pg_notify('enabled_image_changed', $1)")
            .bind(image_uri)
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
        // Issue #535 (a): best-effort NOTIFY (mirrors org_secret_changed's
        // delete path — no open transaction to ride here).
        let _ = sqlx::query("SELECT pg_notify('enabled_image_changed', $1)")
            .bind(image_uri)
            .execute(&self.pool)
            .await;
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
                   last_refreshed_at, created_at, updated_at, soft_deleted_at,
                   capture_env
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
        capture_env: &[engram_core::types::CaptureEnvEntry],
    ) -> Result<EnableJob, MetaError> {
        // INSERT guarded by the partial unique index (one non-terminal
        // job per image_uri); on conflict fall through to SELECTing
        // the in-flight job. Re-POST = resume, never duplicate work.
        // `capture_env` rides the job so the scanner injects it into the
        // [warm] hook at capture (refs resolved there, values never stored).
        let inserted = sqlx::query(
            r#"
            INSERT INTO enable_jobs (id, image_uri, manifest_digest, capture_env)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (image_uri) WHERE state NOT IN ('ready', 'failed')
            DO NOTHING
            RETURNING id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, capture_env, capture_phase, warm_stage, warm_stage_started_at, warm_stages, output_tail, prestage_hosts, created_at, updated_at
            "#,
        )
        .bind(Uuid::new_v4())
        .bind(image_uri)
        .bind(manifest_digest)
        .bind(sqlx::types::Json(capture_env))
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        if let Some(row) = inserted {
            return row::enable_job_from_row(&row);
        }
        let existing = sqlx::query(
            r#"
            SELECT id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, capture_env, capture_phase, warm_stage, warm_stage_started_at, warm_stages, output_tail, prestage_hosts, created_at, updated_at
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
                    INSERT INTO enable_jobs (id, image_uri, manifest_digest, capture_env)
                    VALUES ($1, $2, $3, $4)
                    ON CONFLICT (image_uri) WHERE state NOT IN ('ready', 'failed')
                    DO NOTHING
                    RETURNING id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, capture_env, capture_phase, warm_stage, warm_stage_started_at, warm_stages, output_tail, prestage_hosts, created_at, updated_at
                    "#,
                )
                .bind(Uuid::new_v4())
                .bind(image_uri)
                .bind(manifest_digest)
                .bind(sqlx::types::Json(capture_env))
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
            r#"SELECT id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, capture_env, capture_phase, warm_stage, warm_stage_started_at, warm_stages, output_tail, prestage_hosts, created_at, updated_at FROM enable_jobs WHERE id = $1"#,
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
            SELECT id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, capture_env, capture_phase, warm_stage, warm_stage_started_at, warm_stages, output_tail, prestage_hosts, created_at, updated_at
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
            RETURNING id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, capture_env, capture_phase, warm_stage, warm_stage_started_at, warm_stages, output_tail, prestage_hosts, created_at, updated_at
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
        claimant: &str,
        chunks_done: u32,
        chunks_total: Option<u32>,
    ) -> Result<(), MetaError> {
        // Fenced by `claimed_by`: a checkpoint (which also renews the
        // lease via `claimed_at = NOW()`) only lands while the caller
        // still holds the claim. An expired-lease pod whose job was
        // re-claimed by a peer would otherwise reset the new
        // claimant's `chunks_done` and renew the lease on its behalf
        // — see #232.
        let res = sqlx::query(
            r#"
            UPDATE enable_jobs
               SET chunks_done = $3,
                   chunks_total = COALESCE($4, chunks_total),
                   claimed_at = NOW(),
                   updated_at = NOW()
             WHERE id = $1 AND claimed_by = $2
            "#,
        )
        .bind(id)
        .bind(claimant)
        .bind(chunks_done as i32)
        .bind(chunks_total.map(|v| v as i32))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(self.enable_job_fence_miss(id, claimant).await);
        }
        Ok(())
    }

    /// Issue #539: persist one `CaptureProgress` event onto the job row.
    /// `warm_stage_started_at` is derived from `progress.warm_stages`' still-
    /// open entry (the caller — `PooledBackend::run_warm_hook`/
    /// `build_base_snapshot` via the coordinator's stream consumer — always
    /// sends the full stage history, not a diff), not re-derived in SQL, so
    /// a stage-name-unchanged heartbeat doesn't need a `DISTINCT FROM`
    /// dance to avoid resetting it.
    ///
    /// Fenced by `claimed_by` exactly like [`Self::update_enable_job_progress`]
    /// — see #232. Also renews the claim (`claimed_at = NOW()`), which is
    /// what lets the enable-scanner delete its blind capture-lease ticker
    /// (`enable_scanner.rs`): the host's >=30s keepalive comfortably beats
    /// the 300s lease.
    async fn update_enable_job_capture_progress(
        &self,
        id: Uuid,
        claimant: &str,
        progress: &engram_core::types::CaptureProgress,
    ) -> Result<(), MetaError> {
        let warm_stage_started_at = progress
            .warm_stages
            .iter()
            .find(|s| s.ended_at.is_none())
            .map(|s| s.started_at);
        let res = sqlx::query(
            r#"
            UPDATE enable_jobs
               SET capture_phase = $3,
                   warm_stage = $4,
                   warm_stage_started_at = $5,
                   warm_stages = $6,
                   output_tail = $7,
                   claimed_at = NOW(),
                   updated_at = NOW()
             WHERE id = $1 AND claimed_by = $2
            "#,
        )
        .bind(id)
        .bind(claimant)
        .bind(progress.phase.as_str())
        .bind(progress.warm_stage.as_deref())
        .bind(warm_stage_started_at)
        .bind(sqlx::types::Json(&progress.warm_stages))
        .bind(&progress.output_tail)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(self.enable_job_fence_miss(id, claimant).await);
        }
        Ok(())
    }

    async fn set_enable_job_state(
        &self,
        id: Uuid,
        claimant: &str,
        state: EnableJobState,
    ) -> Result<(), MetaError> {
        // Terminal states release the claim; non-failed targets clear
        // any stale error from a prior retried attempt. Fenced by
        // `claimed_by` (#232): a stale pod must not flip the state of
        // a job a peer now owns, nor release that peer's claim.
        let res = sqlx::query(
            r#"
            UPDATE enable_jobs
               SET state = $3,
                   error = CASE WHEN $3 = 'failed' THEN error ELSE NULL END,
                   claimed_by = CASE WHEN $3 IN ('ready', 'failed') THEN NULL ELSE claimed_by END,
                   claimed_at = CASE WHEN $3 IN ('ready', 'failed') THEN NULL ELSE NOW() END,
                   updated_at = NOW()
             WHERE id = $1 AND claimed_by = $2
            "#,
        )
        .bind(id)
        .bind(claimant)
        .bind(state.as_str())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(self.enable_job_fence_miss(id, claimant).await);
        }
        Ok(())
    }

    async fn record_enable_job_failure(
        &self,
        id: Uuid,
        claimant: &str,
        error: &str,
        max_attempts: u32,
        force_terminal: bool,
    ) -> Result<(u32, EnableJobState), MetaError> {
        // ONE atomic, fenced write: bump attempts, store error, release
        // the claim, and flip to `failed` in the SAME statement iff the
        // budget is spent OR the failure is non-retryable. Doing the flip
        // here (not a follow-up set_enable_job_state) is load-bearing —
        // this write nulls `claimed_by`, so a separate fenced flip would
        // fence-miss and silently fail, leaving the job non-terminal to be
        // re-claimed and re-failed forever (the runaway-attempts bug).
        //
        // The CASE reads the pre-bump `attempts`, so `attempts + 1` is the
        // post-bump count on both lines. Fenced by `claimed_by` (#232): a
        // stale pod's transient error must not clear a peer's lease or stamp
        // `error` on a job that peer is actively completing.
        let row: Option<(i32, String)> = sqlx::query_as(
            r#"
            UPDATE enable_jobs
               SET attempts = attempts + 1,
                   error = $3,
                   state = CASE
                             WHEN $5 OR attempts + 1 >= $4 THEN 'failed'
                             ELSE state
                           END,
                   claimed_by = NULL,
                   claimed_at = NULL,
                   updated_at = NOW()
             WHERE id = $1 AND claimed_by = $2
            RETURNING attempts, state
            "#,
        )
        .bind(id)
        .bind(claimant)
        .bind(error)
        .bind(max_attempts as i32)
        .bind(force_terminal)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        match row {
            Some((attempts, state)) => {
                let state = row::parse_enable_job_state(&state)?;
                Ok((attempts.max(0) as u32, state))
            }
            None => Err(self.enable_job_fence_miss(id, claimant).await),
        }
    }

    async fn retry_enable_job(&self, id: Uuid) -> Result<EnableJob, MetaError> {
        // Issue #539 (migration 0079): also reset the capture-progress
        // columns a prior (failed) capture attempt left behind. Without
        // this, a retried capture starts fresh but the UI kept rendering
        // the PREVIOUS attempt's `capture_phase`/`warm_stage`/
        // `warm_stage_started_at`/`warm_stages`/`output_tail` as if it
        // were live, until the new attempt's first `CaptureProgress`
        // event overwrote them (or forever, if the retry fails before
        // emitting one).
        let row = sqlx::query(
            r#"
            UPDATE enable_jobs
               SET state = 'pending', attempts = 0, error = NULL,
                   claimed_by = NULL, claimed_at = NULL, updated_at = NOW(),
                   capture_phase = NULL, warm_stage = NULL,
                   warm_stage_started_at = NULL, warm_stages = NULL,
                   output_tail = NULL
             WHERE id = $1 AND state = 'failed'
            RETURNING id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, capture_env, capture_phase, warm_stage, warm_stage_started_at, warm_stages, output_tail, prestage_hosts, created_at, updated_at
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

    // ---- ADR 0036 amendment: fleet chunk prestage (issue #538) ----

    async fn begin_enable_job_prestage(
        &self,
        id: Uuid,
        claimant: &str,
        prestage_ref: serde_json::Value,
    ) -> Result<(), MetaError> {
        // Stamp the wire-shape ref the heartbeat ack advertises to hosts,
        // and flip to `prestaging` in the SAME fenced write (mirrors
        // `set_enable_job_state`: renews the claim, fenced by `claimed_by`
        // — #232 semantics — so a stale pod can't advertise a ref for a
        // job a peer now owns).
        let res = sqlx::query(
            r#"
            UPDATE enable_jobs
               SET state = 'prestaging',
                   prestage_ref = $3,
                   claimed_at = NOW(),
                   updated_at = NOW()
             WHERE id = $1 AND claimed_by = $2
            "#,
        )
        .bind(id)
        .bind(claimant)
        .bind(prestage_ref)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(self.enable_job_fence_miss(id, claimant).await);
        }
        Ok(())
    }

    async fn set_enable_job_prestage_hosts(
        &self,
        id: Uuid,
        claimant: &str,
        outcomes: serde_json::Value,
    ) -> Result<(), MetaError> {
        // Written once at the end of the prestage wait — the audit /
        // dashboard record of per-host staged|timed_out|unschedulable
        // outcomes. Fenced by `claimed_by` (#232): a stale pod's stragglers
        // must not overwrite a peer's in-progress or completed record.
        let res = sqlx::query(
            r#"
            UPDATE enable_jobs
               SET prestage_hosts = $3,
                   claimed_at = NOW(),
                   updated_at = NOW()
             WHERE id = $1 AND claimed_by = $2
            "#,
        )
        .bind(id)
        .bind(claimant)
        .bind(outcomes)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(self.enable_job_fence_miss(id, claimant).await);
        }
        Ok(())
    }

    async fn list_prestaging_refs(&self) -> Result<Vec<serde_json::Value>, MetaError> {
        // Raw JSON out — engram-core (and this store) must not depend on
        // engram-protocol (the wire-type crate depends on core, not the
        // reverse); the coordinator's heartbeat handler deserializes each
        // value into `EnabledImageRef`, matching the existing dependency
        // direction rather than laundering a stringly type here.
        let rows: Vec<(serde_json::Value,)> = sqlx::query_as(
            r#"
            SELECT prestage_ref FROM enable_jobs
             WHERE state = 'prestaging' AND prestage_ref IS NOT NULL
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows.into_iter().map(|(v,)| v).collect())
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

    // ---- session capabilities (ADR 0056) ----

    async fn bind_session_capabilities(
        &self,
        session_id: SessionId,
        caps: &[Capability],
    ) -> Result<(), MetaError> {
        // No-op on empty (the queued-then-booted re-prepare carries an empty
        // set; the rows were bound at enqueue). ON CONFLICT DO NOTHING keeps
        // it idempotent across the boot/enqueue double-bind. `resource` ''
        // is the no-resource sentinel (NULL can't sit in the PK).
        if caps.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        for c in caps {
            sqlx::query(
                r#"
                INSERT INTO session_capabilities (session_id, provider, action, resource)
                VALUES ($1, $2, $3, $4)
                ON CONFLICT DO NOTHING
                "#,
            )
            .bind(session_id.as_uuid())
            .bind(&c.provider)
            .bind(&c.action)
            .bind(c.resource.as_deref().unwrap_or(""))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn get_session_capabilities(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<Capability>, MetaError> {
        let rows = sqlx::query(
            r#"
            SELECT provider, action, resource FROM session_capabilities
            WHERE session_id = $1
            ORDER BY provider, action, resource
            "#,
        )
        .bind(session_id.as_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let provider: String = sqlx::Row::try_get(r, "provider").map_err(db_err)?;
            let action: String = sqlx::Row::try_get(r, "action").map_err(db_err)?;
            let resource: String = sqlx::Row::try_get(r, "resource").map_err(db_err)?;
            out.push(Capability {
                provider,
                action,
                resource: (!resource.is_empty()).then_some(resource),
            });
        }
        Ok(out)
    }

    // ---- session integration policy (ADR 0056 B′) ----

    async fn bind_session_integration_policy(
        &self,
        session_id: SessionId,
        policy_json: &str,
    ) -> Result<(), MetaError> {
        sqlx::query(
            r#"
            INSERT INTO session_integration_policy (session_id, policy_json)
            VALUES ($1, $2)
            ON CONFLICT (session_id) DO UPDATE SET policy_json = EXCLUDED.policy_json
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(policy_json)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_session_integration_policy(
        &self,
        session_id: SessionId,
    ) -> Result<Option<String>, MetaError> {
        let row =
            sqlx::query("SELECT policy_json FROM session_integration_policy WHERE session_id = $1")
                .bind(session_id.as_uuid())
                .fetch_optional(&self.pool)
                .await
                .map_err(db_err)?;
        match row {
            Some(r) => Ok(Some(sqlx::Row::try_get(&r, "policy_json").map_err(db_err)?)),
            None => Ok(None),
        }
    }

    async fn get_session_harness(
        &self,
        session_id: SessionId,
    ) -> Result<Option<String>, MetaError> {
        let row = sqlx::query("SELECT harness FROM sessions WHERE id = $1")
            .bind(session_id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        match row {
            Some(r) => Ok(sqlx::Row::try_get(&r, "harness").map_err(db_err)?),
            None => Ok(None),
        }
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

    async fn release_session_lease(
        &self,
        session_id: SessionId,
        locked_by: &str,
    ) -> Result<bool, MetaError> {
        // Scoped to the holder: `AND locked_by = $2`. Without it, a holder
        // whose row was reaped (held >180s without a touch) and then
        // re-acquired by another holder would blind-delete the new
        // holder's lease on its own late Drop — the serializer fails open
        // and two pipelines drive one session. Mirrors the touch's filter.
        let res = sqlx::query("DELETE FROM session_lease WHERE session_id = $1 AND locked_by = $2")
            .bind(session_id.as_uuid())
            .bind(locked_by)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        // Idempotent: missing row = already released / reaped / stolen.
        Ok(res.rows_affected() == 1)
    }

    async fn session_lease_held(&self, session_id: SessionId) -> Result<bool, MetaError> {
        let row: Option<(uuid::Uuid,)> =
            sqlx::query_as("SELECT session_id FROM session_lease WHERE session_id = $1")
                .bind(session_id.as_uuid())
                .fetch_optional(&self.pool)
                .await
                .map_err(db_err)?;
        Ok(row.is_some())
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

    /// ADR 0035 §5 + ADR 0055 P2 + ADR 0062: distinct bundle generations
    /// referenced by any snapshot row **∪ every live `mount_catalog` skill** ∪
    /// **the current harness catalog generation** (`dyn_0`) — so a
    /// registered-but-currently-unused uploaded skill, and the harness catalog a
    /// fresh session will mount, both stay staged on the fleet (their sha enters
    /// `live_bundles`) and survive the bundle GC. jsonb unnest
    /// in SQL so the coord never pages either table; the result is a handful of
    /// refs. Catalog pins carry the skill `name` as a cosmetic `drive_id` (the
    /// host stages by `sha256`); a sha pinned by both a snapshot slot and the
    /// catalog appears once per distinct drive_id, which the GC (keyed on sha)
    /// and the host supervisor (stages by sha, idempotent) both collapse.
    async fn bundle_pin_set(
        &self,
    ) -> Result<Vec<engram_core::types::sandbox::AuxBundleRef>, MetaError> {
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT DISTINCT b->>'drive_id', b->>'sha256'
               FROM snapshots, jsonb_array_elements(aux_bundles) AS b
             UNION
             SELECT name, sha256 FROM mount_catalog WHERE deleted_at IS NULL
             UNION
             SELECT name, squashfs_sha256 FROM harness_catalog WHERE deleted_at IS NULL",
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

    /// ADR 0055 P2: upsert-by-name a packed user-uploaded skill into the
    /// org-shared catalog. A name matching a live row updates it in place
    /// (stable `id` + `created_at`); content-addressing makes re-registering the
    /// same bytes a no-op-equivalent (same `sha256`). Returns the live row.
    async fn register_skill(
        &self,
        owner: &str,
        name: &str,
        description: &str,
        sha256: &str,
        mount_json: &str,
        size_bytes: i64,
    ) -> Result<engram_core::types::CatalogSkill, MetaError> {
        let row = sqlx::query_as::<
            _,
            (
                String,
                String,
                String,
                String,
                String,
                String,
                i64,
                chrono::DateTime<chrono::Utc>,
            ),
        >(
            "INSERT INTO mount_catalog (owner, name, description, sha256, mount_json, size_bytes)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (name) WHERE deleted_at IS NULL
             DO UPDATE SET owner = EXCLUDED.owner,
                           description = EXCLUDED.description,
                           sha256 = EXCLUDED.sha256,
                           mount_json = EXCLUDED.mount_json,
                           size_bytes = EXCLUDED.size_bytes
             RETURNING id, owner, name, description, sha256, mount_json, size_bytes, created_at",
        )
        .bind(owner)
        .bind(name)
        .bind(description)
        .bind(sha256)
        .bind(mount_json)
        .bind(size_bytes)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(catalog_skill_from_row(row))
    }

    /// ADR 0055 P2: every live catalog skill, newest first.
    async fn list_skills(&self) -> Result<Vec<engram_core::types::CatalogSkill>, MetaError> {
        let rows = sqlx::query_as::<
            _,
            (
                String,
                String,
                String,
                String,
                String,
                String,
                i64,
                chrono::DateTime<chrono::Utc>,
            ),
        >(
            "SELECT id, owner, name, description, sha256, mount_json, size_bytes, created_at
               FROM mount_catalog WHERE deleted_at IS NULL
             ORDER BY created_at DESC, name ASC",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows.into_iter().map(catalog_skill_from_row).collect())
    }

    /// ADR 0055 P2: resolve one selected skill name to its live catalog row.
    async fn get_skill_by_name(
        &self,
        name: &str,
    ) -> Result<Option<engram_core::types::CatalogSkill>, MetaError> {
        let row = sqlx::query_as::<
            _,
            (
                String,
                String,
                String,
                String,
                String,
                String,
                i64,
                chrono::DateTime<chrono::Utc>,
            ),
        >(
            "SELECT id, owner, name, description, sha256, mount_json, size_bytes, created_at
               FROM mount_catalog WHERE name = $1 AND deleted_at IS NULL",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(row.map(catalog_skill_from_row))
    }

    /// ADR 0055 P2: soft-delete a catalog skill (drops it from the pin set →
    /// existing bundle GC reclaims the blob after grace). Returns whether a live
    /// row was deleted.
    async fn soft_delete_skill(&self, name: &str) -> Result<bool, MetaError> {
        let res = sqlx::query(
            "UPDATE mount_catalog SET deleted_at = now()
               WHERE name = $1 AND deleted_at IS NULL",
        )
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
    }

    /// ADR 0062: upsert-by-name a harness into the org-shared catalog. A name
    /// matching a live row updates it in place (stable `id` + `created_at`).
    async fn register_harness(
        &self,
        reg: engram_core::types::HarnessRegistration<'_>,
    ) -> Result<engram_core::types::CatalogHarness, MetaError> {
        let row = sqlx::query_as::<_, HarnessRow>(&format!(
            "INSERT INTO harness_catalog
                 (owner, name, oci_ref, manifest_digest, descriptor_toml, squashfs_sha256, squashfs_size_bytes)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (name) WHERE deleted_at IS NULL
             DO UPDATE SET owner = EXCLUDED.owner,
                           oci_ref = EXCLUDED.oci_ref,
                           manifest_digest = EXCLUDED.manifest_digest,
                           descriptor_toml = EXCLUDED.descriptor_toml,
                           squashfs_sha256 = EXCLUDED.squashfs_sha256,
                           squashfs_size_bytes = EXCLUDED.squashfs_size_bytes
             RETURNING {HARNESS_ROW_COLS}"
        ))
        .bind(reg.owner)
        .bind(reg.name)
        .bind(reg.oci_ref)
        .bind(reg.manifest_digest)
        .bind(reg.descriptor_toml)
        .bind(reg.squashfs_sha256)
        .bind(reg.squashfs_size_bytes)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(catalog_harness_from_row(row))
    }

    /// ADR 0062: every live catalog harness, newest first.
    async fn list_harnesses(&self) -> Result<Vec<engram_core::types::CatalogHarness>, MetaError> {
        let rows = sqlx::query_as::<_, HarnessRow>(&format!(
            "SELECT {HARNESS_ROW_COLS} FROM harness_catalog WHERE deleted_at IS NULL
             ORDER BY created_at DESC, name ASC"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows.into_iter().map(catalog_harness_from_row).collect())
    }

    /// ADR 0062: resolve one harness name to its live catalog row.
    async fn get_harness_by_name(
        &self,
        name: &str,
    ) -> Result<Option<engram_core::types::CatalogHarness>, MetaError> {
        let row = sqlx::query_as::<_, HarnessRow>(&format!(
            "SELECT {HARNESS_ROW_COLS} FROM harness_catalog WHERE name = $1 AND deleted_at IS NULL"
        ))
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(row.map(catalog_harness_from_row))
    }

    /// ADR 0062: soft-delete a custom catalog harness. Its squashfs leaves the
    /// pin set (`bundle_pin_set`) and the bundle GC reclaims it once unpinned.
    async fn soft_delete_harness(&self, name: &str) -> Result<bool, MetaError> {
        let res = sqlx::query(
            "UPDATE harness_catalog SET deleted_at = now()
               WHERE name = $1 AND deleted_at IS NULL",
        )
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
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
