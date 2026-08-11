//! Postgres-backed [`MetadataStore`] implementation.
//!
//! Queries are written against the schema in `deploy/migrations/0001_initial.sql`.
//! sqlx's compile-time checking is intentionally disabled here — runtime
//! queries let the crate build without a live database, which keeps the
//! workspace usable for early-stage development. We can flip to
//! `query!`/`query_as!` once CI provisions a Postgres service and we
//! ship a `.sqlx/` cache.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use engram_core::traits::clock::{Clock, Entropy, OsEntropy, SystemClock};
use engram_core::traits::{
    CreateDisposition, DisableEnabledImageOutcome, ExecLifecycleEventKind, ExecOutputStream,
    MetadataStore, SessionCreateWriteSet,
};
use engram_core::types::session_op::{EnqueueOutcome, OpKind, OpState, SessionOp};
use engram_core::types::{
    ArtifactRow, BindingDisposition, Capability, CaptureJobAssignment, CaptureJobReport,
    CaptureJobRow, CaptureTerminalReport, ColdBaseRow, EnableJob, EnableJobState, EnabledImage,
    EventCursor, HostRecord, HostStatus, NewCaptureJob, PersistedEvent, RegistryCredential,
    Session, SessionSecrets, SessionSpec, SessionState, SnapshotRecord,
};
use engram_core::{CaptureJobId, HostId, MetaError, SandboxId, SessionId, SnapshotId};
use row::col_err;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;
use uuid::Uuid;

mod row;

#[derive(Clone)]
pub struct PostgresStore {
    pool: PgPool,
    /// Time source (ADR 0098 D3). Every SQL `now()` in this crate became a
    /// bind parameter fed from this clock, and every Rust-side wall-clock
    /// read (`Utc::now()`) reads it too — so no time decision originates
    /// inside Postgres and live-PG tests are time-controllable.
    clock: Arc<dyn Clock>,
    /// Entropy source (ADR 0098 D1/D3). The prod id-minting paths
    /// (`create_session`, enable/capture job inserts) draw their `Uuid`s
    /// here rather than calling `Uuid::new_v4()` directly, so the D4
    /// conformance suite can seed them to match `SimMetadataStore`.
    entropy: Arc<dyn Entropy>,
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
        Ok(Self {
            pool,
            clock: Arc::new(SystemClock::new()),
            entropy: Arc::new(OsEntropy),
        })
    }

    /// Inject a [`Clock`] (ADR 0098 D3/D4). Production takes the default
    /// [`SystemClock`]; the D4 conformance suite and time-controlled
    /// live-PG tests pass a fake so SQL-bound time is deterministic.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Inject an [`Entropy`] (ADR 0098 D3/D4). Production takes the
    /// default [`OsEntropy`]; the D4 conformance suite seeds it so the
    /// ids this store mints replay to match `SimMetadataStore`.
    pub fn with_entropy(mut self, entropy: Arc<dyn Entropy>) -> Self {
        self.entropy = entropy;
        self
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
    /// fired for every discrete placement-feasibility event. These events
    /// include a freed reservation, a released `pending` reservation, a host
    /// registration, a changed host scheduling vector, an uncordoned host,
    /// and a newly queued session. The scanner LISTENs on
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

    /// ADR 0079: one enqueue transaction — INSERT the op row, and (when
    /// `claimed_by` is `Some` and the fresh row is immediately runnable:
    /// nothing running, nothing queued ahead) claim it inline by
    /// CAS-bumping `sessions.current_epoch` and stamping the row
    /// `running`. `pg_notify('session_ops', session_id)` fires in the
    /// same transaction for both Inserted outcomes so every replica's
    /// executor wakes on commit.
    ///
    /// A racing claim from another pod trips the `session_ops_one_running`
    /// partial unique index and fails THIS transaction (insert included)
    /// with 23505 — [`MetadataStore::op_enqueue_and_claim`] detects that
    /// and retries once with `claimed_by = None` (enqueue-only).
    async fn op_enqueue_tx(
        &self,
        session_id: SessionId,
        kind: OpKind,
        payload: &serde_json::Value,
        idempotency_key: Option<&str>,
        claimed_by: Option<&str>,
    ) -> Result<EnqueueOutcome, MetaError> {
        let now = self.clock.now_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        // ON CONFLICT must name the partial-unique's predicate to arbitrate
        // on `session_ops_idem`; a NULL key row is never indexed, so it
        // can never be a duplicate. The predicate is scoped to the ACTIVE
        // states (ADR 0079 review finding #4): the fresh row is inserted
        // `queued` (in the index), so it conflicts with an existing
        // queued|running keyed row (→ Duplicate) but NOT with a terminal
        // one — a terminal keyed row no longer burns the key, so the
        // scanner/reaper can re-enqueue after a terminal failure.
        let insert = format!(
            "INSERT INTO session_ops (session_id, kind, payload, idempotency_key)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (session_id, kind, idempotency_key)
                 WHERE idempotency_key IS NOT NULL AND state IN ('queued', 'running')
                 DO NOTHING
             RETURNING {OP_COLUMNS}"
        );
        let row = sqlx::query(&insert)
            .bind(session_id.as_uuid())
            .bind(kind.as_str())
            .bind(payload)
            .bind(idempotency_key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
        let Some(row) = row else {
            // Idempotency hit: the identical (session, kind, key) row
            // already exists — the enqueue is a no-op, and the existing
            // row's own enqueue already notified.
            tx.rollback().await.map_err(db_err)?;
            return Ok(EnqueueOutcome::Duplicate);
        };
        let op = row::session_op_from_row(&row)?;

        // Inline claim IFF nothing is running for the session AND the
        // just-inserted row is the queue head (it is always due: a fresh
        // row has `not_before` NULL). This SELECT is advisory — the
        // one_running partial unique is what makes a racing claim fail.
        let claimable = match claimed_by {
            None => false,
            Some(_) => sqlx::query_scalar::<_, bool>(
                "SELECT NOT EXISTS (SELECT 1 FROM session_ops
                                     WHERE session_id = $1 AND state = 'running')
                    AND NOT EXISTS (SELECT 1 FROM session_ops
                                     WHERE session_id = $1 AND state = 'queued' AND id < $2)",
            )
            .bind(session_id.as_uuid())
            .bind(op.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?,
        };

        let outcome = if let (true, Some(claimant)) = (claimable, claimed_by) {
            let epoch: i64 = sqlx::query_scalar(
                "UPDATE sessions SET current_epoch = current_epoch + 1
                 WHERE id = $1 RETURNING current_epoch",
            )
            .bind(session_id.as_uuid())
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            let stamp = format!(
                "UPDATE session_ops
                    SET state = 'running', epoch = $2, claimed_by = $3,
                        claimed_at = $4, heartbeat_at = $4,
                        attempts = attempts + 1
                  WHERE id = $1 AND state = 'queued'
                 RETURNING {OP_COLUMNS}"
            );
            let row = sqlx::query(&stamp)
                .bind(op.id)
                .bind(epoch)
                .bind(claimant)
                .bind(now)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)?;
            EnqueueOutcome::Claimed(row::session_op_from_row(&row)?)
        } else {
            EnqueueOutcome::Queued(op)
        };

        sqlx::query("SELECT pg_notify('session_ops', $1)")
            .bind(session_id.as_uuid().to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(outcome)
    }
}

fn db_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> MetaError {
    MetaError::Db(Box::new(e))
}

/// The `status IN (…)` body for the host-memory-reserving states, built
/// from the ONE authoritative set,
/// [`SessionState::host_memory_reserving_states`], instead of a
/// hand-spelled literal per query.
///
/// Literal drift here is an incident class, not a hypothetical: ADR 0101
/// Phase C added `parked` to the typed set, and the five hand-rolled SQL
/// twins in this file all missed it — so a parked survivor was absent
/// from the register-time rehydrate list, its NBD device was never
/// re-served after a host-agent roll, and the quarantine ladder destroyed
/// a healthy paused VM (session 61a03b7e, 2026-07-21: 93 events rewound).
/// No query spells this list by hand any more.
fn reserving_states_sql() -> String {
    engram_core::types::session::SessionState::host_memory_reserving_states()
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",")
}

/// ADR 0079: PG unique-violation (SQLSTATE 23505). The op-claim paths
/// lean on the `session_ops_one_running` partial unique index for
/// correctness — a racing second claimer fails its transaction here, and
/// the caller maps that to "not claimed" instead of an error.
fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).as_deref() == Some("23505")
}

/// The [`is_unique_violation`] probe lifted over [`MetaError`], for call
/// sites that only see the already-wrapped error.
fn meta_is_unique_violation(e: &MetaError) -> bool {
    matches!(e, MetaError::Db(b)
        if b.downcast_ref::<sqlx::Error>().is_some_and(is_unique_violation))
}

/// ADR 0079: the full `session_ops` projection, kept in ONE place so the
/// six queries that materialize a [`SessionOp`] can't drift from
/// `row::session_op_from_row`'s strict decode. (`claimed_at` is a
/// DB-side bookkeeping column the domain type doesn't carry.)
const OP_COLUMNS: &str = "id, session_id, kind, payload, state, step, epoch, attempts, \
     not_before, idempotency_key, claimed_by, error, created_at, finished_at";

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

/// Per-candidate no-fit classification — the diagnostic twin of
/// [`choose_placement_host`], sharing its exact fit arithmetic so the
/// reported reason can never disagree with the pick. Bounded reason
/// vocabulary per [`engram_core::traits::PlacementNoFit`]. Pure so it's
/// unit-tested without a database.
fn classify_no_fit(
    candidates: &[uuid::Uuid],
    fit: &std::collections::HashMap<uuid::Uuid, HostFit>,
    budget_mib: i64,
    budget_vcpus: i64,
) -> Vec<engram_core::traits::PlacementNoFit> {
    candidates
        .iter()
        .map(|h| {
            let Some(f) = fit.get(h) else {
                return engram_core::traits::PlacementNoFit {
                    host_id: HostId(*h),
                    reason: "not_lockable",
                    free_mib: 0,
                    free_vcpus: 0,
                };
            };
            let free_vcpus = if f.cpu_budget > 0 {
                f.cpu_budget - f.reserved_vcpus
            } else {
                i64::MAX
            };
            let free_mib = f.alloc_mib - f.reserved_mib;
            let reason = if f.alloc_mib <= 0 {
                "unmeasured"
            } else if free_mib < budget_mib {
                "ram_full"
            } else if free_vcpus < budget_vcpus {
                "cpu_full"
            } else {
                // Fits at read time — the pick ran earlier under FOR
                // UPDATE and lost a race; honest label beats a lie.
                "fits_now"
            };
            engram_core::traits::PlacementNoFit {
                host_id: HostId(*h),
                reason,
                free_mib,
                free_vcpus,
            }
        })
        .collect()
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

/// ADR 0046/0048/0081: the shared `FOR UPDATE` 2D pick every reserving
/// placer runs — session create (`reserve_and_persist_create`), the
/// queue scanner (`place_queued_session`), and capture reservation
/// (`place_capture_job` / `reassign_capture_job`). Locks the candidate host rows in PK order
/// so all placers (any replica) serialize on the overlap without
/// deadlock, builds the fit map from host-measured `allocatable_mib`
/// (0 = unmeasured → soft) + the vCPU budget, subtracts the reserved
/// SUM — memory-reserving sessions UNION capturing enable jobs (ADR
/// 0081: sessions and captures are mutually visible) — and best-fit
/// chooses via [`choose_placement_host`].
///
/// `Ok(None)` = nothing fits (or no candidates); the caller decides
/// queue-vs-reject and owns the commit/rollback — the lock is held for
/// the REST of the caller's transaction.
async fn pick_host_2d(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    cand: &[uuid::Uuid],
    affinity_len: usize,
    budget_mib: i64,
    budget_vcpus: i64,
) -> Result<Option<uuid::Uuid>, MetaError> {
    if cand.is_empty() {
        return Ok(None);
    }
    let host_rows = sqlx::query(
        r#"
        SELECT id, allocatable_mib, total_vcpus
        FROM hosts
        WHERE id = ANY($1) AND status IN ('ready','draining') AND NOT cordoned
        -- ORDER BY id BEFORE `FOR UPDATE`: every placer (any replica)
        -- locks the overlapping host rows in the SAME (PK) order, so a
        -- burst can't lock {A,B} vs {B,A} and deadlock. The LockRows
        -- executor node sits atop the sort, so rows are locked in id
        -- order. (Load test: `deadlock detected` under concurrent
        -- creates before this.)
        ORDER BY id
        FOR UPDATE
        "#,
    )
    .bind(cand)
    .fetch_all(&mut **tx)
    .await
    .map_err(db_err)?;
    // ADR 0046/0048: build the 2D fit map. allocatable_mib is the
    // host-measured RAM headroom (nets out daemon/OS/chunk-cache/mlock
    // baseline; 0 = unmeasured). The CPU budget is total_vcpus ×
    // overcommit (0 = host hasn't reported its core count → no CPU gate).
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
    // Reserved within the txn — sees the committed reservations of
    // placers that locked these hosts before us. The sessions branch is
    // the SQL twin of `SessionState::host_memory_reserving_states()`
    // (interpolated from the const — see `reserving_states_sql`);
    // the enable_jobs branch is ADR 0081's capture-VM reservation.
    let res_sql = format!(
        r#"
        SELECT host_id,
               COALESCE(SUM(mem), 0)::BIGINT AS reserved_mib,
               COALESCE(SUM(cpu), 0)::BIGINT AS reserved_vcpus
        FROM (
            SELECT host_id, mem_budget_mib AS mem, cpu_budget_vcpus::BIGINT AS cpu
            FROM sessions
            WHERE host_id = ANY($1)
              AND status IN ({reserving})
              -- R3 (#722): ONE reservation authority — a `pending` pinned to
              -- a host reserves its budget UNCONDITIONALLY, for exactly as
              -- long as it is `pending`. No wall-age / live-op exclusion: an
              -- aged crash-orphan physically holds its slot until it LEAVES
              -- the reserving state, and the ADR 0079 pending-orphan backstop
              -- reclaims it by a REAL `pending → failed` transition (the sole
              -- reclaimer) — never by a placement-side exclusion. The old
              -- 10-minute crash-orphan gate stopped counting an aged pending
              -- while the backstop could still revive it, so a revival landed
              -- on re-sold capacity (Σ reserved > allocatable). This predicate
              -- now equals the unconditional placement-accounting oracle by
              -- construction (engram-dst invariants.rs).
            UNION ALL
            -- ADR 0084 (c): a capture VM reserves like a session. The
            -- reservation lives on `capture_jobs` now (moved off
            -- `enable_jobs`); a non-terminal row with a bound host_id
            -- holds its budget. Release is implicit — a terminal stage
            -- drops out of this SUM.
            SELECT host_id, mem_budget_mib, cpu_budget_vcpus::BIGINT
            FROM capture_jobs
            WHERE host_id = ANY($1)
              AND stage NOT IN ('done','failed')
        ) reserved
        GROUP BY host_id
        "#,
        reserving = reserving_states_sql(),
    );
    let res_rows = sqlx::query(&res_sql)
        .bind(cand)
        .fetch_all(&mut **tx)
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
    Ok(choose_placement_host(
        cand,
        affinity_len,
        &fit,
        budget_mib,
        budget_vcpus,
    ))
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

    /// The diagnostic classifier must agree with the pick: whenever
    /// `choose_placement_host` returns `None`, every candidate needs a
    /// non-`fits_now` reason, and the reason must name the binding
    /// dimension.
    #[test]
    fn classify_no_fit_names_the_binding_dimension() {
        let h = ids(4);
        let mut fit = ram_fit(&[
            (h[0], 8192, 6000), // free 2192 < 4096 → ram_full
            (h[1], 0, 0),       // unmeasured
        ]);
        fit.insert(
            h[2],
            HostFit {
                alloc_mib: 32768,
                reserved_mib: 0,
                cpu_budget: 8,
                reserved_vcpus: 8, // free 0 < 2 → cpu_full
            },
        );
        // h[3] deliberately absent from the map → not_lockable.
        let details = super::classify_no_fit(&h, &fit, 4096, 2);
        let by_id: HashMap<Uuid, &str> = details
            .iter()
            .map(|d| (d.host_id.as_uuid(), d.reason))
            .collect();
        assert_eq!(by_id[&h[0]], "ram_full");
        assert_eq!(by_id[&h[1]], "unmeasured");
        assert_eq!(by_id[&h[2]], "cpu_full");
        assert_eq!(by_id[&h[3]], "not_lockable");
        let ram = details
            .iter()
            .find(|d| d.host_id.as_uuid() == h[0])
            .unwrap();
        assert_eq!(ram.free_mib, 2192);
    }

    /// A host that fits at diagnostic-read time reports the honest
    /// `fits_now` (the pick lost a race) rather than inventing a reason.
    #[test]
    fn classify_no_fit_reports_fits_now_on_race() {
        let h = ids(1);
        let fit = ram_fit(&[(h[0], 8192, 0)]);
        let details = super::classify_no_fit(&h, &fit, 4096, 0);
        assert_eq!(details[0].reason, "fits_now");
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

impl PostgresStore {
    /// The shared INSERT-or-idempotent-UPDATE body behind `record_snapshot`
    /// / `fenced_record_snapshot` (ADR 0079 re-review #3/#4). `fence = None`
    /// writes unconditionally; `Some(epoch)` gates the write on the
    /// session's `current_epoch` atomically. Returns `None` when fenced
    /// (nothing written), else `Some(inserted)`.
    async fn record_snapshot_guarded(
        &self,
        snap: SnapshotRecord,
        fence: Option<i64>,
    ) -> Result<Option<bool>, MetaError> {
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
        let now = self.clock.now_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        // ADR 0079 (re-review findings #3/#4): the optional fence. When
        // `Some(epoch)`, gate the write on the session's `current_epoch`
        // in the SAME transaction (the fence read + the write serialize
        // against a concurrent claim/reclaim CAS via `FOR UPDATE`). A
        // mismatch means the op executor was fenced by a successor's
        // re-claim: write NOTHING (a reclaimed-out predecessor must never
        // land a phantom `recoverable` row a resume would pick — the
        // 89f7984d durability-lie class) and return `Ok(None)`.
        if let Some(epoch) = fence {
            let session_id = snap.session_id.ok_or_else(|| {
                MetaError::Serialization(
                    "fenced_record_snapshot requires a session-scoped snapshot".into(),
                )
            })?;
            let stored: Option<i64> =
                sqlx::query_scalar("SELECT current_epoch FROM sessions WHERE id = $1 FOR UPDATE")
                    .bind(session_id.as_uuid())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db_err)?;
            if stored != Some(epoch) {
                let _ = tx.rollback().await;
                return Ok(None);
            }
        }
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
                updated_at              = $16
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
        .bind(now)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let inserted: bool = sqlx::Row::try_get(&row, "inserted").map_err(db_err)?;
        sqlx::query("UPDATE chunk_generation SET generation = generation + 1 WHERE id = TRUE")
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        // ADR 0077 phase 1: advance the per-session durable head in the
        // SAME transaction as the row write — row existence ==
        // durability. Monotonic by created_at: a re-record or an
        // out-of-order reconcile (the host re-advertises an older
        // checkpoint) never regresses the head. Base captures
        // (session_id IS NULL) skip this — their head is
        // enabled_images.base_snapshot_id.
        //
        // GATED ON `recoverable`: the issue-#213 two-phase capture and the
        // resume demote path both record rows with recoverable=false, and
        // the head's contract is "newest snapshot whose blobs AND row are
        // committed" — an unrecoverable row must never hold it (the next
        // abort-inflight tick deletes its blobs while the monotonic guard
        // would block an older good snapshot from ever reclaiming the
        // pointer). A demote that hits the CURRENT head re-points it to
        // the newest still-recoverable snapshot instead.
        if let Some(session_id) = snap.session_id {
            if snap.recoverable {
                sqlx::query(
                    r#"
                    UPDATE sessions
                    SET durable_head_snapshot_id = $2
                    WHERE id = $1
                      AND (
                        durable_head_snapshot_id IS NULL
                        OR $3 >= COALESCE(
                            (SELECT created_at FROM snapshots WHERE id = sessions.durable_head_snapshot_id),
                            'epoch'::timestamptz
                        )
                      )
                    "#,
                )
                .bind(session_id.as_uuid())
                .bind(snap.id.as_uuid())
                .bind(snap.created_at)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            } else {
                sqlx::query(
                    r#"
                    UPDATE sessions
                    SET durable_head_snapshot_id = (
                        SELECT id FROM snapshots
                        WHERE session_id = $1 AND recoverable AND id <> $2
                        ORDER BY created_at DESC
                        LIMIT 1
                    )
                    WHERE id = $1 AND durable_head_snapshot_id = $2
                    "#,
                )
                .bind(session_id.as_uuid())
                .bind(snap.id.as_uuid())
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            }
        }
        tx.commit().await.map_err(db_err)?;
        Ok(Some(inserted))
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
        let id = self.entropy.uuid();
        let now = self.clock.now_utc();
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
        // But existing != still-`pending`: a DeleteSession (the destroy op,
        // ADR 0079) can flip the row terminal while the restore RPC that
        // preceded this call is in flight. Guard on the
        // expected state and check `rows_affected` so a lost race surfaces
        // as `NotFound` instead of silently binding `sandbox_id` onto
        // whatever status the row now has — the caller's `Err` arm tears the
        // now-orphaned sandbox back down.
        let res = sqlx::query(
            r#"
            UPDATE sessions
               SET status = 'created', sandbox_id = $2, last_active_at = $3
             WHERE id = $1 AND status = 'pending'
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(sandbox_id.as_uuid())
        .bind(self.clock.now_utc())
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
        let now = self.clock.now_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        // -------- pick a host (ADR 0046/0048 best-fit 2D), if any candidate --------
        // Issue #535 (b): `pick_host_2d` is `reserve_placement`'s FOR-UPDATE
        // pick (now shared with `place_queued_session` and ADR 0084's
        // `place_capture_job` / `reassign_capture_job`) — extended below so the SAME transaction
        // also writes the satellites instead of stopping at the bare row
        // insert. The candidate host-row locks are held for the REST of the
        // transaction, not just the pick + insert — `tx.commit()` is at the
        // bottom of this function, after the sealed-secrets insert, the
        // per-capability insert loop, and the integration-policy upsert all
        // run on the same `tx`. A many-capability create serializes
        // concurrent placers on that whole multi-round-trip critical
        // section, not a sub-ms window.
        let cand: Vec<uuid::Uuid> = candidates.iter().map(|h| h.as_uuid()).collect();
        let picked: Option<uuid::Uuid> = pick_host_2d(
            &mut tx,
            &cand,
            affinity_len,
            ws.mem_budget_mib,
            ws.cpu_budget_vcpus as i64,
        )
        .await?;

        let disposition = match picked {
            Some(host) => {
                sqlx::query(
                    r#"
                    INSERT INTO sessions
                        (id, status, host_id, sandbox_id,
                         image_uri, mode, mem_budget_mib, cpu_budget_vcpus,
                         harness,
                         created_at, last_active_at)
                    VALUES ($1, 'pending', $2, NULL, $3, $4, $5, $6, $7, $8, $8)
                    "#,
                )
                .bind(ws.session_id.as_uuid())
                .bind(host)
                .bind(&ws.spec.image)
                .bind(ws.spec.mode.as_str())
                .bind(ws.mem_budget_mib)
                .bind(ws.cpu_budget_vcpus)
                // ADR 0077: `harness` mirrors the RuntimeSpec's selection into
                // the pre-existing `sessions.harness` column; `selected_skills`
                // is no longer a column — it lives in the RuntimeSpec written
                // below (migration 0090 dropped the column).
                .bind(ws.runtime_spec.selected_harness.as_deref())
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
                         harness,
                         queued_at, queue_origin,
                         created_at, last_active_at)
                    VALUES ($1, 'queued', NULL, NULL, $2, $3, $4, $5, $6, $7, 'create', $7, $7)
                    "#,
                )
                .bind(ws.session_id.as_uuid())
                .bind(&ws.spec.image)
                .bind(ws.spec.mode.as_str())
                .bind(ws.mem_budget_mib)
                .bind(ws.cpu_budget_vcpus)
                // ADR 0077: harness column mirrors the RuntimeSpec; no
                // selected_skills column (lives in the RuntimeSpec below).
                .bind(ws.runtime_spec.selected_harness.as_deref())
                .bind(now)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                CreateDisposition::Queued
            }
        };

        // -------- satellites, in the SAME transaction (issue #535 (b)) --------
        // `harness` already rode the row INSERT above (mirrored from the
        // RuntimeSpec); the remaining satellites keep their own tables (FK'd
        // to `sessions.id`, now guaranteed to exist by the time this commits).
        //
        // ADR 0077 phase 3: the RuntimeSpec (selected skills/harness/workdir)
        // is written in THIS transaction too — the single durable source the
        // queue re-prepare / resume / evac reads instead of re-deriving. It
        // subsumes the retired `sessions.selected_skills` column.
        {
            let spec_json = serde_json::to_value(&ws.runtime_spec)
                .map_err(|e| MetaError::Serialization(format!("runtime_spec encode: {e}")))?;
            sqlx::query(
                r#"
                INSERT INTO session_runtime_specs (session_id, spec, updated_at)
                VALUES ($1, $2, $3)
                ON CONFLICT (session_id) DO UPDATE
                  SET spec = EXCLUDED.spec, updated_at = $3
                "#,
            )
            .bind(ws.session_id.as_uuid())
            .bind(spec_json)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
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
        if let Some(binding) = &ws.oauth_binding {
            let result = sqlx::query(
                r#"
                INSERT INTO session_oauth_bindings
                    (session_id, subject_kind, subject_id, provider)
                SELECT $1, $2, $3, $4
                FROM oauth_credentials
                WHERE subject_kind=$2 AND subject_id=$3 AND provider=$4
                  AND revoked_at IS NULL
                "#,
            )
            .bind(ws.session_id.as_uuid())
            .bind(binding.key.subject_kind.as_str())
            .bind(&binding.key.subject_id)
            .bind(&binding.key.provider)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            if result.rows_affected() != 1 {
                return Err(MetaError::Conflict(
                    "session OAuth binding requires a live credential".into(),
                ));
            }
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
                 teleport_target_set_at = CASE WHEN $2 IS NULL THEN NULL ELSE $3 END \
             WHERE id = $1",
        )
        .bind(id.as_uuid())
        .bind(target.map(|h| h.as_uuid()))
        .bind(self.clock.now_utc())
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
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (name) DO UPDATE SET
                wrapped_dek = EXCLUDED.wrapped_dek,
                nonce       = EXCLUDED.nonce,
                ciphertext  = EXCLUDED.ciphertext,
                key_id      = EXCLUDED.key_id,
                updated_at  = $6
            RETURNING name, key_id, created_at, updated_at
            "#,
        )
        .bind(&sealed.name)
        .bind(&sealed.wrapped_dek)
        .bind(&sealed.nonce)
        .bind(&sealed.ciphertext)
        .bind(&sealed.key_id)
        .bind(self.clock.now_utc())
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

    // ---- ADR 0106: subject-scoped OAuth credentials and flow state ----

    async fn put_oauth_credential(
        &self,
        credential: engram_core::types::oauth::NewSealedOAuthCredential,
        expected_version: Option<i64>,
    ) -> Result<engram_core::types::oauth::SealedOAuthCredential, MetaError> {
        let now = self.clock.now_utc();
        let metadata = serde_json::to_value(&credential.metadata)
            .map_err(|e| MetaError::Serialization(e.to_string()))?;
        let row = if let Some(expected) = expected_version {
            sqlx::query(
                r#"
                UPDATE oauth_credentials SET
                    wrapped_dek = $4, nonce = $5, ciphertext = $6, key_id = $7,
                    account_metadata = $8, version = version + 1,
                    updated_at = $9, revoked_at = NULL, expires_at = $11,
                    refresh_claim_until = NULL, broken_at = NULL, broken_reason = NULL
                WHERE subject_kind = $1 AND subject_id = $2 AND provider = $3
                  AND version = $10
                RETURNING *
                "#,
            )
            .bind(credential.key.subject_kind.as_str())
            .bind(&credential.key.subject_id)
            .bind(&credential.key.provider)
            .bind(&credential.wrapped_dek)
            .bind(&credential.nonce)
            .bind(&credential.ciphertext)
            .bind(&credential.key_id)
            .bind(metadata)
            .bind(now)
            .bind(expected)
            .bind(credential.expires_at)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?
        } else {
            sqlx::query(
                r#"
                INSERT INTO oauth_credentials (
                    subject_kind, subject_id, provider, wrapped_dek, nonce,
                    ciphertext, key_id, account_metadata, version, created_at,
                    updated_at, revoked_at, expires_at
                ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,1,$9,$9,NULL,$10)
                ON CONFLICT (subject_kind, subject_id, provider) DO NOTHING
                RETURNING *
                "#,
            )
            .bind(credential.key.subject_kind.as_str())
            .bind(&credential.key.subject_id)
            .bind(&credential.key.provider)
            .bind(&credential.wrapped_dek)
            .bind(&credential.nonce)
            .bind(&credential.ciphertext)
            .bind(&credential.key_id)
            .bind(metadata)
            .bind(now)
            .bind(credential.expires_at)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?
        };
        match row {
            Some(row) => oauth_credential_from_pg(&row),
            None => Err(MetaError::Conflict(format!(
                "OAuth credential CAS rejected for {}/{}/{}",
                credential.key.subject_kind.as_str(),
                credential.key.subject_id,
                credential.key.provider
            ))),
        }
    }

    async fn get_oauth_credential(
        &self,
        key: &engram_core::types::oauth::OAuthCredentialKey,
    ) -> Result<Option<engram_core::types::oauth::SealedOAuthCredential>, MetaError> {
        sqlx::query(
            "SELECT * FROM oauth_credentials WHERE subject_kind=$1 AND subject_id=$2 AND provider=$3",
        )
        .bind(key.subject_kind.as_str())
        .bind(&key.subject_id)
        .bind(&key.provider)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .map(|row| oauth_credential_from_pg(&row))
        .transpose()
    }

    async fn list_oauth_credentials(
        &self,
        subject_kind: engram_core::types::oauth::OAuthSubjectKind,
        subject_id: Option<&str>,
    ) -> Result<Vec<engram_core::types::oauth::SealedOAuthCredential>, MetaError> {
        sqlx::query(
            r#"
            SELECT * FROM oauth_credentials
            WHERE subject_kind=$1 AND ($2::text IS NULL OR subject_id=$2)
            ORDER BY subject_id, provider
            "#,
        )
        .bind(subject_kind.as_str())
        .bind(subject_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?
        .iter()
        .map(oauth_credential_from_pg)
        .collect()
    }

    async fn revoke_oauth_credential(
        &self,
        key: &engram_core::types::oauth::OAuthCredentialKey,
        expected_version: i64,
    ) -> Result<engram_core::types::oauth::SealedOAuthCredential, MetaError> {
        let row = sqlx::query(
            r#"
            UPDATE oauth_credentials SET revoked_at=$4, updated_at=$4,
                version=version+1
            WHERE subject_kind=$1 AND subject_id=$2 AND provider=$3
              AND version=$5 AND revoked_at IS NULL
            RETURNING *
            "#,
        )
        .bind(key.subject_kind.as_str())
        .bind(&key.subject_id)
        .bind(&key.provider)
        .bind(self.clock.now_utc())
        .bind(expected_version)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        match row {
            Some(row) => oauth_credential_from_pg(&row),
            None => {
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM oauth_credentials WHERE subject_kind=$1 AND subject_id=$2 AND provider=$3)",
                )
                .bind(key.subject_kind.as_str())
                .bind(&key.subject_id)
                .bind(&key.provider)
                .fetch_one(&self.pool)
                .await
                .map_err(db_err)?;
                if exists {
                    Err(MetaError::Conflict("OAuth credential CAS rejected".into()))
                } else {
                    Err(MetaError::NotFound)
                }
            }
        }
    }

    async fn create_oauth_flow(
        &self,
        flow: engram_core::types::oauth::OAuthFlow,
    ) -> Result<(), MetaError> {
        let result = sqlx::query(
            r#"
            INSERT INTO oauth_flows (
                id, subject_kind, subject_id, provider, owner_replica,
                lease_expires_at, expires_at, status, error_code, created_at, updated_at
            ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
            "#,
        )
        .bind(flow.id)
        .bind(flow.key.subject_kind.as_str())
        .bind(&flow.key.subject_id)
        .bind(&flow.key.provider)
        .bind(&flow.owner_replica)
        .bind(flow.lease_expires_at)
        .bind(flow.expires_at)
        .bind(flow.status.as_str())
        .bind(&flow.error_code)
        .bind(flow.created_at)
        .bind(flow.updated_at)
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(e)
                if e.as_database_error()
                    .is_some_and(|e| e.is_unique_violation()) =>
            {
                Err(MetaError::Conflict(
                    "an OAuth flow is already pending for this subject and provider".into(),
                ))
            }
            Err(e) => Err(db_err(e)),
        }
    }

    async fn get_oauth_flow(
        &self,
        id: uuid::Uuid,
    ) -> Result<Option<engram_core::types::oauth::OAuthFlow>, MetaError> {
        sqlx::query("SELECT * FROM oauth_flows WHERE id=$1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?
            .map(|row| oauth_flow_from_pg(&row))
            .transpose()
    }

    async fn renew_oauth_flow_lease(
        &self,
        id: uuid::Uuid,
        owner_replica: &str,
        lease_expires_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), MetaError> {
        let now = self.clock.now_utc();
        let updated = sqlx::query(
            r#"
            UPDATE oauth_flows SET lease_expires_at=$3, updated_at=$4
            WHERE id=$1 AND owner_replica=$2 AND status='pending'
              AND lease_expires_at > $4 AND expires_at > $4
            "#,
        )
        .bind(id)
        .bind(owner_replica)
        .bind(lease_expires_at)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if updated.rows_affected() == 1 {
            Ok(())
        } else {
            Err(MetaError::Conflict("OAuth flow owner lease changed".into()))
        }
    }

    async fn get_session_oauth_binding(
        &self,
        session_id: SessionId,
    ) -> Result<Option<engram_core::types::oauth::SessionOAuthBinding>, MetaError> {
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT subject_kind, subject_id, provider FROM session_oauth_bindings WHERE session_id=$1",
        )
        .bind(session_id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|(kind, subject_id, provider)| {
            use std::str::FromStr;
            Ok(engram_core::types::oauth::SessionOAuthBinding {
                session_id,
                key: engram_core::types::oauth::OAuthCredentialKey {
                    subject_kind: engram_core::types::oauth::OAuthSubjectKind::from_str(&kind)
                        .map_err(MetaError::Serialization)?,
                    subject_id,
                    provider,
                },
            })
        })
        .transpose()
    }

    async fn finish_oauth_flow(
        &self,
        id: uuid::Uuid,
        owner_replica: &str,
        status: engram_core::types::oauth::OAuthFlowStatus,
        error_code: Option<&str>,
    ) -> Result<(), MetaError> {
        if !status.is_terminal() {
            return Err(MetaError::Conflict(
                "flow finish status must be terminal".into(),
            ));
        }
        let now = self.clock.now_utc();
        let updated = sqlx::query(
            r#"
            UPDATE oauth_flows SET status=$3, error_code=$4, updated_at=$5
            WHERE id=$1 AND owner_replica=$2 AND status='pending'
              AND lease_expires_at > $5
            "#,
        )
        .bind(id)
        .bind(owner_replica)
        .bind(status.as_str())
        .bind(error_code)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if updated.rows_affected() == 1 {
            Ok(())
        } else {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM oauth_flows WHERE id=$1)")
                    .bind(id)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(db_err)?;
            if exists {
                Err(MetaError::Conflict(
                    "OAuth flow owner lease or status changed".into(),
                ))
            } else {
                Err(MetaError::NotFound)
            }
        }
    }

    async fn get_pending_oauth_flow(
        &self,
        key: &engram_core::types::oauth::OAuthCredentialKey,
    ) -> Result<Option<engram_core::types::oauth::OAuthFlow>, MetaError> {
        sqlx::query(
            "SELECT * FROM oauth_flows WHERE subject_kind=$1 AND subject_id=$2 AND provider=$3 AND status='pending'",
        )
        .bind(key.subject_kind.as_str())
        .bind(&key.subject_id)
        .bind(&key.provider)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .map(|row| oauth_flow_from_pg(&row))
        .transpose()
    }

    async fn finish_oauth_flow_unowned(
        &self,
        id: uuid::Uuid,
        status: engram_core::types::oauth::OAuthFlowStatus,
        error_code: Option<&str>,
    ) -> Result<(), MetaError> {
        if !status.is_terminal() {
            return Err(MetaError::Conflict(
                "flow finish status must be terminal".into(),
            ));
        }
        let now = self.clock.now_utc();
        let updated = sqlx::query(
            r#"
            UPDATE oauth_flows SET status=$2, error_code=$3, updated_at=$4
            WHERE id=$1 AND status='pending' AND expires_at > $4
            "#,
        )
        .bind(id)
        .bind(status.as_str())
        .bind(error_code)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if updated.rows_affected() == 1 {
            Ok(())
        } else {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM oauth_flows WHERE id=$1)")
                    .bind(id)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(db_err)?;
            if exists {
                Err(MetaError::Conflict(
                    "OAuth flow is no longer pending or has expired".into(),
                ))
            } else {
                Err(MetaError::NotFound)
            }
        }
    }

    async fn list_oauth_credentials_due_for_refresh(
        &self,
        kind: engram_core::types::oauth::OAuthSubjectKind,
        now: chrono::DateTime<chrono::Utc>,
        due_before: chrono::DateTime<chrono::Utc>,
        limit: i64,
    ) -> Result<Vec<engram_core::types::oauth::SealedOAuthCredential>, MetaError> {
        sqlx::query(
            r#"
            SELECT * FROM oauth_credentials
            WHERE subject_kind = $1
              AND expires_at IS NOT NULL AND expires_at <= $2
              AND revoked_at IS NULL AND broken_at IS NULL
              AND (refresh_claim_until IS NULL OR refresh_claim_until < $3)
            ORDER BY expires_at ASC, subject_id, provider
            LIMIT $4
            "#,
        )
        .bind(kind.as_str())
        .bind(due_before)
        .bind(now)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?
        .iter()
        .map(oauth_credential_from_pg)
        .collect()
    }

    async fn claim_oauth_refresh(
        &self,
        key: &engram_core::types::oauth::OAuthCredentialKey,
        now: chrono::DateTime<chrono::Utc>,
        until: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool, MetaError> {
        let updated = sqlx::query(
            r#"
            UPDATE oauth_credentials SET refresh_claim_until = $5
            WHERE subject_kind=$1 AND subject_id=$2 AND provider=$3
              AND revoked_at IS NULL AND broken_at IS NULL
              AND (refresh_claim_until IS NULL OR refresh_claim_until < $4)
            "#,
        )
        .bind(key.subject_kind.as_str())
        .bind(&key.subject_id)
        .bind(&key.provider)
        .bind(now)
        .bind(until)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(updated.rows_affected() == 1)
    }

    async fn mark_oauth_credential_broken(
        &self,
        key: &engram_core::types::oauth::OAuthCredentialKey,
        expected_version: i64,
        reason: &str,
    ) -> Result<engram_core::types::oauth::SealedOAuthCredential, MetaError> {
        let now = self.clock.now_utc();
        let row = sqlx::query(
            r#"
            UPDATE oauth_credentials SET
                broken_at=$5, broken_reason=$6, refresh_claim_until=NULL, updated_at=$5
            WHERE subject_kind=$1 AND subject_id=$2 AND provider=$3
              AND version=$4 AND revoked_at IS NULL AND broken_at IS NULL
            RETURNING *
            "#,
        )
        .bind(key.subject_kind.as_str())
        .bind(&key.subject_id)
        .bind(&key.provider)
        .bind(expected_version)
        .bind(now)
        .bind(reason)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        match row {
            Some(row) => oauth_credential_from_pg(&row),
            None => {
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM oauth_credentials WHERE subject_kind=$1 AND subject_id=$2 AND provider=$3)",
                )
                .bind(key.subject_kind.as_str())
                .bind(&key.subject_id)
                .bind(&key.provider)
                .fetch_one(&self.pool)
                .await
                .map_err(db_err)?;
                if exists {
                    Err(MetaError::Conflict(
                        "OAuth credential version moved; reload the winner".into(),
                    ))
                } else {
                    Err(MetaError::NotFound)
                }
            }
        }
    }

    async fn cleanup_oauth_flows(
        &self,
        now: chrono::DateTime<chrono::Utc>,
        delete_before: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64, MetaError> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let changed = sqlx::query(
            r#"
            UPDATE oauth_flows SET
              status = CASE WHEN expires_at <= $1 THEN 'expired' ELSE 'owner_lost' END,
              error_code = CASE WHEN expires_at <= $1 THEN 'flow_expired' ELSE 'owner_lost' END,
              updated_at = $1
            WHERE status='pending' AND (expires_at <= $1 OR lease_expires_at <= $1)
            "#,
        )
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        let deleted =
            sqlx::query("DELETE FROM oauth_flows WHERE status <> 'pending' AND updated_at < $1")
                .bind(delete_before)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?
                .rows_affected();
        tx.commit().await.map_err(db_err)?;
        Ok(changed + deleted)
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

    async fn enqueue_session_resume(&self, id: SessionId, epoch: i64) -> Result<bool, MetaError> {
        // Idle → queued (resume origin). Gated on `status='idle'` so a
        // racing resume that already advanced the row is a clean no-op,
        // AND on the resume op's fencing epoch (ADR 0079) so a
        // reclaimed-away zombie executor can't fork the state machine —
        // same predicate shape as `fenced_transition_session`.
        let n = sqlx::query(
            r#"
            UPDATE sessions
               SET status = 'queued', queued_at = $3, queue_origin = 'resume',
                   last_active_at = $3
             WHERE id = $1 AND status = 'idle' AND current_epoch = $2
            "#,
        )
        .bind(id.as_uuid())
        .bind(epoch)
        .bind(self.clock.now_utc())
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
        Ok(n > 0)
    }

    async fn enqueue_evacuating_session_resume(
        &self,
        id: SessionId,
        epoch: i64,
    ) -> Result<bool, MetaError> {
        // #800: Evacuating → queued (resume origin), the RESERVED
        // evac-placement overflow path. Same fenced-CAS shape as
        // `enqueue_session_resume` above, but gated on `status='evacuating'`
        // (the evac resumer's input state) instead of `'idle'`. The row
        // keeps its `mem_budget_mib` / `cpu_budget_vcpus` from create, so
        // the queue scanner's resume precheck fits it against the SAME hard
        // 2D bound. A no-op (0 rows) means a peer already relocated the
        // session or the epoch moved — the caller stops without emitting the
        // Queued event.
        let n = sqlx::query(
            r#"
            UPDATE sessions
               SET status = 'queued', queued_at = $3, queue_origin = 'resume',
                   last_active_at = $3
             WHERE id = $1 AND status = 'evacuating' AND current_epoch = $2
            "#,
        )
        .bind(id.as_uuid())
        .bind(epoch)
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n > 0 {
            self.notify_placement_changed("enqueued").await;
        }
        Ok(n > 0)
    }

    async fn list_queued_sessions_fifo(
        &self,
    ) -> Result<Vec<engram_core::types::session::QueuedSession>, MetaError> {
        let rows = sqlx::query(
            r#"
            SELECT id, status, host_id, sandbox_id, image_uri, mode,
                   created_at, last_active_at,
                   live_disk_manifest_id, live_disk_manifest_version,
                   COALESCE(mem_budget_mib, 0)::BIGINT AS mem_budget_mib,
                   COALESCE(cpu_budget_vcpus, 0) AS cpu_budget_vcpus,
                   queue_origin, queued_at
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
        let now = self.clock.now_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        // Same FOR UPDATE serialization + 2D fit as every reserving placer
        // (shared `pick_host_2d`, ADR 0046/0048/0081).
        let Some(picked) = pick_host_2d(
            &mut tx,
            &cand,
            affinity_len,
            mem_budget_mib,
            cpu_budget_vcpus as i64,
        )
        .await?
        else {
            tx.rollback().await.map_err(db_err)?;
            return Ok(None);
        };
        // Flip queued → pending on the picked host. 0 rows = lost a race
        // (already left `queued`); roll back, report no placement.
        let n = sqlx::query(
            r#"
            UPDATE sessions
               SET status = 'pending', host_id = $2, last_active_at = $3
             WHERE id = $1 AND status = 'queued'
            "#,
        )
        .bind(id.as_uuid())
        .bind(picked)
        .bind(now)
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

    // ADR 0079 (issue #543): `requeue_session` / `requeue_stale_pending`
    // retired — the create_boot op's `not_before`/`attempts` own boot
    // retry, and the op reclaim sweep owns coord-died-mid-boot recovery.

    async fn queued_demand(&self) -> Result<engram_core::types::session::QueuedDemand, MetaError> {
        // ADR 0084 (c): waiting captures (`capture_jobs` rows with no host
        // bound yet — `host_id IS NULL`, non-terminal) fold into the
        // queued-session demand — the K4 autoscaler scales up for a
        // capture exactly as for a session, and its scale-down hard gate
        // (`queued_sessions == 0`) holds the fleet while one waits.
        let row: (i64, i64, i64) = sqlx::query_as(
            r#"
            SELECT COUNT(*)::BIGINT,
                   COALESCE(SUM(mem), 0)::BIGINT,
                   COALESCE(SUM(cpu), 0)::BIGINT
            FROM (
                SELECT mem_budget_mib AS mem, cpu_budget_vcpus::BIGINT AS cpu
                FROM sessions WHERE status = 'queued'
                UNION ALL
                SELECT mem_budget_mib, cpu_budget_vcpus::BIGINT
                FROM capture_jobs
                WHERE host_id IS NULL
                  AND stage NOT IN ('done','failed')
            ) demand
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
        // Same predicate as `pick_host_2d` / `fleet_free_mib` — the
        // memory-reserving states (R3 #722: a `pending` reserves
        // UNCONDITIONALLY until it leaves that state; no crash-orphan gate),
        // UNION the capturing enable jobs (ADR 0081) — summing BOTH budget
        // dimensions (ADR 0048).
        let sql = format!(
            r#"
            SELECT host_id,
                   COALESCE(SUM(mem), 0)::BIGINT,
                   COALESCE(SUM(cpu), 0)::BIGINT
            FROM (
                SELECT host_id, mem_budget_mib AS mem, cpu_budget_vcpus::BIGINT AS cpu
                FROM sessions
                WHERE host_id IS NOT NULL
                  AND status IN ({reserving})
                UNION ALL
                -- ADR 0084 (c): capturing VMs reserve on `capture_jobs`.
                SELECT host_id, mem_budget_mib, cpu_budget_vcpus::BIGINT
                FROM capture_jobs
                WHERE host_id IS NOT NULL
                  AND stage NOT IN ('done','failed')
            ) reserved
            GROUP BY host_id
            "#,
            reserving = reserving_states_sql(),
        );
        let rows: Vec<(uuid::Uuid, i64, i64)> = sqlx::query_as(&sql)
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
                   created_at, last_active_at, last_event_at,
                   live_disk_manifest_id, live_disk_manifest_version,
                   park_rung, parked_at, suggested_title
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

    /// ADR 0090: reverse ownership — non-terminal states only. `host_lost`
    /// is deliberately INCLUDED (its surviving VM is what recovery is for);
    /// a terminal row owns nothing.
    async fn session_owning_sandbox(
        &self,
        host_id: HostId,
        sandbox_id: engram_core::SandboxId,
    ) -> Result<Option<SessionId>, MetaError> {
        let row: Option<uuid::Uuid> = sqlx::query_scalar(
            r#"
            SELECT id FROM sessions
            WHERE sandbox_id = $1 AND host_id = $2
              AND status NOT IN ('failed','completed','dead')
            LIMIT 1
            "#,
        )
        .bind(sandbox_id.as_uuid())
        .bind(host_id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(row.map(SessionId))
    }

    /// Diagnostic twin of `pick_host_2d`, without the `FOR UPDATE`: same
    /// fit-map construction (same status/cordon predicate, same reserved
    /// SUM incl. capture jobs — R3 #722: a `pending` reserves
    /// unconditionally, no crash-orphan gate), then the pure
    /// [`classify_no_fit`]. Runs only on the no-capacity path.
    async fn placement_no_fit_details(
        &self,
        candidates: &[HostId],
        mem_budget_mib: i64,
        cpu_budget_vcpus: i32,
    ) -> Result<Vec<engram_core::traits::PlacementNoFit>, MetaError> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let cand: Vec<uuid::Uuid> = candidates.iter().map(|h| h.as_uuid()).collect();
        let host_rows = sqlx::query(
            r#"
            SELECT id, allocatable_mib, total_vcpus
            FROM hosts
            WHERE id = ANY($1) AND status IN ('ready','draining') AND NOT cordoned
            "#,
        )
        .bind(&cand)
        .fetch_all(&self.pool)
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
                    cpu_budget: engram_core::types::host::host_cpu_budget(
                        total_vcpus.max(0) as u32
                    ),
                    reserved_vcpus: 0,
                },
            );
        }
        let res_sql = format!(
            r#"
            SELECT host_id,
                   COALESCE(SUM(mem), 0)::BIGINT AS reserved_mib,
                   COALESCE(SUM(cpu), 0)::BIGINT AS reserved_vcpus
            FROM (
                SELECT host_id, mem_budget_mib AS mem, cpu_budget_vcpus::BIGINT AS cpu
                FROM sessions
                WHERE host_id = ANY($1)
                  AND status IN ({reserving})
                  -- R3 (#722): a `pending` reserves UNCONDITIONALLY until it
                  -- leaves the reserving state — no wall-age / live-op gate.
                  -- The ADR 0079 backstop reclaims a true crash-orphan by a
                  -- real `pending → failed` transition (the sole reclaimer),
                  -- so placement never re-sells a slot a revival could reclaim
                  -- (Σ reserved > allocatable). Matches the unconditional
                  -- placement-accounting oracle by construction.
                UNION ALL
                SELECT host_id, mem_budget_mib, cpu_budget_vcpus::BIGINT
                FROM capture_jobs
                WHERE host_id = ANY($1)
                  AND stage NOT IN ('done','failed')
            ) reserved
            GROUP BY host_id
            "#,
            reserving = reserving_states_sql(),
        );
        let res_rows = sqlx::query(&res_sql)
            .bind(&cand)
            .fetch_all(&self.pool)
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
        Ok(classify_no_fit(
            &cand,
            &fit,
            mem_budget_mib,
            cpu_budget_vcpus as i64,
        ))
    }

    async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError> {
        // Every non-terminal state except `host_lost` (limbo pending the
        // reconciler; its bindings are stale by definition) — spelled as
        // the reserving set (from the const, so a new resident state can't
        // drift out of this list) plus `idle`.
        // `evicting` matters most: it keeps `sandbox_id` BOUND while the
        // pipeline runs, so the eviction scanner re-picks a mid-eviction
        // session after a coord roll and its "sandbox no longer bound"
        // guard (which reads `sessions.sandbox_id` directly — ADR 0047,
        // no in-memory registry) still sees the live binding. When
        // `evicting` was missing, a roll mid-eviction dropped the session
        // from the active set, the scanner never re-picked it, and the
        // budget exhausted into a spurious HostLost with the VM still
        // running (prod session 5cfb90b8, 2026-06-03). `parked` likewise
        // holds a paused-in-place VM with its sandbox bound (ADR 0101 C).
        let sql = format!(
            r#"
            SELECT id, status, host_id, sandbox_id,
                   image_uri, mode,
                   created_at, last_active_at, last_event_at,
                   live_disk_manifest_id, live_disk_manifest_version,
                   park_rung, parked_at,
                   suggested_title
            FROM sessions
            WHERE status IN ({reserving},'idle')
            "#,
            reserving = reserving_states_sql(),
        );
        let rows = sqlx::query(&sql)
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
    async fn list_resident_sandboxes_on_host_with_disk_manifest(
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
        // The outer SELECT joins it with the VM-resident sessions
        // on this host and picks max(live, snapshot) version when
        // both share the same manifest_id; if they differ, snapshot
        // wins (mirrors `effective_resume_disk_manifest`'s
        // defensive branch).
        //
        // The status list IS `SessionState::host_memory_reserving_states()`
        // (interpolated from the const): every state whose VM is
        // resident on the host, not just 'active'. A rung-parked
        // 'evicting' session's paused VM survives a host-agent pod
        // roll like any other survivor; filtering it out here left
        // its NBD device unclaimed after the roll — the successor's
        // stale-binding sweep shot the live rootfs and the un-pause
        // resumed the guest onto a dead data plane (session
        // 731df805, 2026-07-17). The same omission recurred when ADR
        // 0101 C introduced 'parked' and this list (then hand-rolled)
        // missed it: the parked survivor fell to the local fallback
        // pass, whose manifest-kind bug quarantined the device and
        // the quarantine ladder destroyed the healthy paused VM
        // (session 61a03b7e, 2026-07-21 — 93 events rewound).
        let sql = format!(
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
              AND s.status IN ({reserving})
              AND s.sandbox_id IS NOT NULL
            "#,
            reserving = reserving_states_sql(),
        );
        let rows = sqlx::query(&sql)
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

    async fn list_resident_sandbox_assignments_on_host(
        &self,
        host_id: HostId,
    ) -> Result<Vec<(SessionId, SandboxId, SessionState)>, MetaError> {
        // ADR 0009 reconcile pass query. Per-host, every heartbeat:
        // ~50 sandboxes/host × 5s cadence × N hosts = trivial DB load.
        // Indexed via `idx_sessions_host_status` (existing). The status
        // set is every RESIDENT (memory-reserving) state — a vanished
        // parked VM must accrue missing strikes like any other
        // (2026-07-21 status-set audit finding 3).
        let sql = format!(
            r#"
            SELECT id, sandbox_id, status
            FROM sessions
            WHERE host_id = $1
              AND status IN ({reserving})
              AND sandbox_id IS NOT NULL
            "#,
            reserving = reserving_states_sql(),
        );
        let rows = sqlx::query(&sql)
            .bind(host_id.as_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let session: Uuid = r
                .try_get("id")
                .map_err(|e| MetaError::Serialization(format!("resident-assignments: id: {e}")))?;
            let sandbox: Uuid = r.try_get("sandbox_id").map_err(|e| {
                MetaError::Serialization(format!("resident-assignments: sandbox_id: {e}"))
            })?;
            let status: String = r.try_get("status").map_err(|e| {
                MetaError::Serialization(format!("resident-assignments: status: {e}"))
            })?;
            out.push((
                SessionId::from(session),
                SandboxId::from(sandbox),
                row::parse_session_state_for_lib(&status)?,
            ));
        }
        Ok(out)
    }

    async fn list_resident_assignments_with_budgets_on_host(
        &self,
        host_id: HostId,
    ) -> Result<Vec<engram_core::types::session::SandboxAssignment>, MetaError> {
        // Every RESIDENT state, not just 'active' — a host holding only
        // parked VMs must not "drain" with an empty list (2026-07-21
        // status-set audit finding 4).
        let sql = format!(
            r#"
            SELECT id, sandbox_id, status,
                   COALESCE(mem_budget_mib, 0)::BIGINT AS mem,
                   COALESCE(cpu_budget_vcpus, 0) AS cpu
            FROM sessions
            WHERE host_id = $1 AND status IN ({reserving}) AND sandbox_id IS NOT NULL
            "#,
            reserving = reserving_states_sql(),
        );
        let rows: Vec<(Uuid, Uuid, String, i64, i32)> = sqlx::query_as(&sql)
            .bind(host_id.as_uuid())
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        rows.into_iter()
            .map(|(s, sb, st, mem, cpu)| {
                Ok(engram_core::types::session::SandboxAssignment {
                    session_id: SessionId::from(s),
                    sandbox_id: SandboxId::from(sb),
                    status: row::parse_session_state_for_lib(&st)?,
                    mem_budget_mib: mem,
                    cpu_budget_vcpus: cpu,
                })
            })
            .collect()
    }

    async fn delete_host(
        &self,
        id: HostId,
    ) -> Result<engram_core::types::session::DeleteHostOutcome, MetaError> {
        use engram_core::types::session::DeleteHostOutcome;
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        // Refuse while any session is still bound — deleting the row out
        // from under a live session would orphan its routing. ADR 0084 (c):
        // an in-flight base-snapshot capture binds the host the same way
        // (its VM is running there); count non-terminal `capture_jobs`
        // rows bound to this host in the same guard.
        let bound_sql = format!(
            r#"
            SELECT (SELECT COUNT(*) FROM sessions
                     WHERE host_id = $1
                       AND status IN ({reserving}))::BIGINT
                 + (SELECT COUNT(*) FROM capture_jobs
                     WHERE host_id = $1
                       AND stage NOT IN ('done','failed'))::BIGINT
            "#,
            reserving = reserving_states_sql(),
        );
        let bound: i64 = sqlx::query_scalar(&bound_sql)
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
        disposition: BindingDisposition,
    ) -> Result<SessionState, MetaError> {
        // SELECT-then-UPDATE under a row-level lock so two concurrent
        // callers can't both validate against the same pre-state. The
        // transaction commits the UPDATE atomically with the lock
        // release.
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let row = sqlx::query(
            r#"
            SELECT status, sandbox_id FROM sessions WHERE id = $1 FOR UPDATE
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
        // #896 / ADR 0090 addendum: the binding disposition is checked
        // under the SAME row lock as the state-pair legality — a bound
        // row arriving where the disposition forbids it is a Conflict,
        // never a silent write.
        let arriving_bound: Option<uuid::Uuid> = row.try_get("sandbox_id").map_err(|e| {
            MetaError::Serialization(format!("transition_session: read sandbox_id: {e}"))
        })?;
        if !target.binding_disposition_legal(arriving_bound.is_some(), disposition) {
            tracing::warn!(
                session_id = %id,
                to = %target.as_str(),
                ?disposition,
                "rejected illegal binding disposition for bound row"
            );
            return Err(MetaError::Conflict(format!(
                "illegal binding disposition {disposition:?} into {} on a bound row",
                target.as_str()
            )));
        }
        // ADR 0018 commit 12b: entering Evacuating resets
        // `evac_attempts` to 0 so a fresh drain (operator or
        // dead-host detector) starts the scanner's retry budget
        // clean. ADR 0034 mirrors this for Evicting/`evict_attempts`.
        // Folded into the same UPDATE that commits the state flip so
        // the counters and the state are always consistent.
        //
        // Entering Queued stamps the queue columns (ADR 0098 D4
        // conformance finding): a bare `transition_session(_, Queued)`
        // is FSM-legal but used to leave `queued_at`/`queue_origin`
        // NULL, and `list_queued_sessions_fifo` then failed to DECODE
        // the row — a scanner-breaking landmine for any future caller.
        // `queued_at` re-stamps (a fresh enqueue moment is what FIFO
        // wants); `queue_origin` keeps an existing origin and defaults
        // to 'create' otherwise.
        sqlx::query(
            r#"
            UPDATE sessions
               SET status = $2,
                   last_active_at = $3,
                   updated_at = $3,
                   sandbox_id = CASE WHEN $4 THEN NULL ELSE sandbox_id END,
                   evac_attempts = CASE WHEN $2 = 'evacuating' THEN 0 ELSE evac_attempts END,
                   evict_attempts = CASE WHEN $2 = 'evicting' THEN 0 ELSE evict_attempts END,
                   queued_at = CASE WHEN $2 = 'queued' THEN $3 ELSE queued_at END,
                   queue_origin = CASE WHEN $2 = 'queued'
                                  THEN COALESCE(queue_origin, 'create')
                                  ELSE queue_origin END
             WHERE id = $1
            "#,
        )
        .bind(id.as_uuid())
        .bind(target.as_str())
        .bind(self.clock.now_utc())
        .bind(matches!(disposition, BindingDisposition::Detach))
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

    async fn list_host_lost_sessions(&self) -> Result<Vec<Session>, MetaError> {
        let rows = sqlx::query(
            r#"
            SELECT id, status, host_id, sandbox_id,
                   image_uri, mode,
                   created_at, last_active_at,
                   live_disk_manifest_id, live_disk_manifest_version,
                   evac_attempts
            FROM sessions
            WHERE status = 'host_lost'
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            out.push(row::session_from_row(r)?);
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
                   park_rung, parked_at,
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

    /// ADR 0101 C: parked-session sweep, via the partial
    /// `idx_sessions_parked` (0107).
    async fn list_parked_sessions(&self) -> Result<Vec<Session>, MetaError> {
        let rows = sqlx::query(
            r#"
            SELECT id, status, host_id, sandbox_id,
                   image_uri, mode,
                   created_at, last_active_at,
                   live_disk_manifest_id, live_disk_manifest_version,
                   park_rung, parked_at,
                   evict_attempts
            FROM sessions
            WHERE status = 'parked'
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row::session_from_row).collect()
    }

    /// ADR 0101 C: the durability-floor settle. One guarded UPDATE —
    /// the status CAS, the sandbox match, and the recoverable-row
    /// EXISTS all evaluate in the same statement, so a racing resume,
    /// rebind, or delete makes this a clean no-op (`false`), never a
    /// partial write. `host_id` is untouched (resume affinity);
    /// `last_active_at` is untouched (it keys the eviction nomination).
    async fn settle_evicted_session_idle(
        &self,
        session_id: SessionId,
        sandbox_id: SandboxId,
        snapshot_id: SnapshotId,
        events: &[(String, serde_json::Value)],
    ) -> Result<Option<Vec<i64>>, MetaError> {
        // One transaction: the guarded settle UPDATE (the CAS — it also
        // row-locks the session for the appends below) plus the event
        // appends. The settle is CAS-once, so events appended by the
        // caller afterwards had a crash window of permanent loss; in-tx
        // they exist iff the settle happened. `pg_notify` fires on
        // commit only.
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let now = self.clock.now_utc();
        let res = sqlx::query(
            r#"
            UPDATE sessions
               SET status = 'idle', sandbox_id = NULL, updated_at = $4
             WHERE id = $1
               AND status = 'evicting'
               AND sandbox_id = $2
               AND EXISTS (
                   SELECT 1 FROM snapshots
                    WHERE id = $3 AND session_id = $1 AND recoverable
               )
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(sandbox_id.as_uuid())
        .bind(snapshot_id.as_uuid())
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        if res.rows_affected() != 1 {
            return Ok(None);
        }
        let mut indices = Vec::with_capacity(events.len());
        for (kind, payload) in events {
            let row = sqlx::query(
                r#"
                WITH next AS (
                    UPDATE sessions
                       SET next_event_idx = next_event_idx + 1,
                           updated_at = $4,
                           last_event_at = $4
                     WHERE id = $1
                 RETURNING next_event_idx - 1 AS allocated_idx, recovery_epoch
                ),
                inserted AS (
                    INSERT INTO session_events (session_id, idx, kind, payload, recovery_epoch, created_at)
                    SELECT $1, allocated_idx, $2, $3, recovery_epoch, $4 FROM next
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
            .bind(now)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            indices.push(sqlx::Row::try_get(&row, "idx").map_err(db_err)?);
        }
        tx.commit().await.map_err(db_err)?;
        // The retired D5 Idle flip fired the queue-scanner wake on
        // `evicting → idle` (a memory-reserving state freeing its
        // budget); the settle inherits it.
        self.notify_placement_changed("session_freed").await;
        Ok(Some(indices))
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
    async fn list_idle_scan_candidates(
        &self,
        _soft_ttl_secs: i64,
        _hard_ttl_secs: i64,
    ) -> Result<Vec<engram_core::traits::metadata::IdleScanCandidate>, MetaError> {
        // One lateral per Active+bound session for its newest event.
        // The TTL params are unused here on purpose: classification
        // (soft vs hard vs neither) is the detector's job; this query
        // returns every Active session's newest-event row and lets the
        // caller cut — the Active set is small (it is the fleet's live
        // VM count), so shipping a few non-candidates is cheaper than
        // splitting the policy across SQL and Rust.
        let rows = sqlx::query(
            r#"
            SELECT s.id, s.sandbox_id, s.host_id, s.shell_pinned_until, s.created_at,
                   le.kind AS last_kind, le.created_at AS last_event_at
            FROM sessions s
            LEFT JOIN LATERAL (
                SELECT e.kind, e.created_at
                FROM session_events e
                WHERE e.session_id = s.id
                ORDER BY e.idx DESC
                LIMIT 1
            ) le ON true
            WHERE s.status = 'active' AND s.sandbox_id IS NOT NULL
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let id: uuid::Uuid = r.try_get("id").map_err(db_err)?;
            let sandbox: Option<uuid::Uuid> = r.try_get("sandbox_id").map_err(db_err)?;
            let host: Option<uuid::Uuid> = r.try_get("host_id").map_err(db_err)?;
            let created_at: chrono::DateTime<chrono::Utc> =
                r.try_get("created_at").map_err(db_err)?;
            let last_event_at: Option<chrono::DateTime<chrono::Utc>> =
                r.try_get("last_event_at").map_err(db_err)?;
            out.push(engram_core::traits::metadata::IdleScanCandidate {
                session_id: SessionId::from(id),
                sandbox_id: sandbox.map(SandboxId::from),
                host_id: host.map(engram_core::HostId::from),
                last_event_at: last_event_at.unwrap_or(created_at),
                last_event_kind: r.try_get("last_kind").map_err(db_err)?,
                shell_pinned_until: r.try_get("shell_pinned_until").map_err(db_err)?,
            });
        }
        Ok(out)
    }

    async fn set_session_park_rung(
        &self,
        id: SessionId,
        rung: i16,
        parked_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<(), MetaError> {
        sqlx::query("UPDATE sessions SET park_rung = $2, parked_at = $3 WHERE id = $1")
            .bind(id.as_uuid())
            .bind(rung)
            .bind(parked_at)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn set_session_suggested_title(
        &self,
        id: SessionId,
        title: &str,
    ) -> Result<(), MetaError> {
        sqlx::query("UPDATE sessions SET suggested_title = $2 WHERE id = $1")
            .bind(id.as_uuid())
            .bind(title)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn fenced_set_session_park_rung(
        &self,
        id: SessionId,
        epoch: i64,
        rung: i16,
        parked_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<bool, MetaError> {
        let res = sqlx::query(
            "UPDATE sessions SET park_rung = $2, parked_at = $3
              WHERE id = $1 AND current_epoch = $4",
        )
        .bind(id.as_uuid())
        .bind(rung)
        .bind(parked_at)
        .bind(epoch)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
    }

    async fn stamp_shell_pin(
        &self,
        id: SessionId,
        pinned_until: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), MetaError> {
        sqlx::query("UPDATE sessions SET shell_pinned_until = $2 WHERE id = $1")
            .bind(id.as_uuid())
            .bind(pinned_until)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

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
                   < $2 - ($1::bigint * INTERVAL '1 second')
            "#,
        )
        .bind(idle_for_secs)
        .bind(self.clock.now_utc())
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
            UPDATE sessions SET host_id = $2, sandbox_id = $3, missing_strikes = 0, updated_at = $4 WHERE id = $1
            "#,
        )
        .bind(id.as_uuid())
        .bind(host_id.as_uuid())
        .bind(sandbox_id.as_uuid())
        .bind(self.clock.now_utc())
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
            UPDATE sessions SET host_id = $2, updated_at = $3 WHERE id = $1
            "#,
        )
        .bind(id.as_uuid())
        .bind(host_id.map(|h| h.as_uuid()))
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n == 0 {
            return Err(MetaError::NotFound);
        }
        Ok(())
    }

    async fn outbox_enqueue(
        &self,
        row: &engram_core::types::outbox::OutboxRow,
    ) -> Result<(), MetaError> {
        // One round trip: idempotent insert + wake every replica's
        // delivery driver. ON CONFLICT DO NOTHING makes caller retries
        // (and cross-pod double-enqueues) harmless.
        sqlx::query(
            "WITH ins AS (
                 INSERT INTO session_outbox
                     (prompt_id, session_id, kind, payload, created_at, not_before)
                 VALUES ($1, $2, $3, $4, $5, $6)
                 ON CONFLICT (prompt_id) DO NOTHING
             )
             SELECT pg_notify('session_outbox', $2::text)",
        )
        .bind(&row.prompt_id)
        .bind(row.session_id.as_uuid())
        .bind(row.kind.as_str())
        .bind(&row.payload)
        .bind(row.created_at)
        .bind(row.not_before)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn append_session_event_and_outbox(
        &self,
        session_id: SessionId,
        kind: &str,
        payload: serde_json::Value,
        outbox: &engram_core::types::outbox::OutboxRow,
    ) -> Result<i64, MetaError> {
        if outbox.session_id != session_id {
            return Err(MetaError::Serialization(
                "event/outbox session ids do not match".into(),
            ));
        }

        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let inserted = sqlx::query(
            r#"
            INSERT INTO session_outbox
                (prompt_id, session_id, kind, payload, created_at, not_before)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (prompt_id) DO NOTHING
            RETURNING prompt_id
            "#,
        )
        .bind(&outbox.prompt_id)
        .bind(session_id.as_uuid())
        .bind(outbox.kind.as_str())
        .bind(&outbox.payload)
        .bind(outbox.created_at)
        .bind(outbox.not_before)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .is_some();

        if !inserted {
            let existing = sqlx::query(
                "SELECT session_id, kind, payload FROM session_outbox WHERE prompt_id = $1",
            )
            .bind(&outbox.prompt_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?
            .ok_or_else(|| {
                MetaError::Conflict(format!(
                    "outbox id {} disappeared during completion",
                    outbox.prompt_id
                ))
            })?;
            let existing_session: uuid::Uuid = existing.try_get("session_id").map_err(db_err)?;
            let existing_kind: String = existing.try_get("kind").map_err(db_err)?;
            let existing_payload: serde_json::Value =
                existing.try_get("payload").map_err(db_err)?;
            if existing_session != session_id.as_uuid()
                || existing_kind != outbox.kind.as_str()
                || existing_payload != outbox.payload
            {
                return Err(MetaError::Conflict(format!(
                    "outbox id {} belongs to another command",
                    outbox.prompt_id
                )));
            }
        }

        let event = sqlx::query(
            r#"
            WITH next AS (
                UPDATE sessions
                   SET next_event_idx = next_event_idx + 1,
                       updated_at = $4,
                       last_event_at = $4
                 WHERE id = $1
             RETURNING next_event_idx - 1 AS allocated_idx, recovery_epoch
            ),
            inserted AS (
                INSERT INTO session_events (session_id, idx, kind, payload, recovery_epoch, created_at)
                SELECT $1, allocated_idx, $2, $3, recovery_epoch, $4 FROM next
                RETURNING idx
            )
            SELECT idx FROM inserted
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(kind)
        .bind(payload)
        .bind(self.clock.now_utc())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .ok_or(MetaError::NotFound)?;
        let idx: i64 = event.try_get("idx").map_err(db_err)?;

        sqlx::query("SELECT pg_notify('session_events', $1), pg_notify('session_outbox', $2)")
            .bind(
                serde_json::json!({ "session_id": session_id.to_string(), "idx": idx }).to_string(),
            )
            .bind(session_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(idx)
    }

    async fn append_events_with_outbox_idempotent(
        &self,
        session_id: SessionId,
        events: &[(String, serde_json::Value)],
        outbox: &engram_core::types::outbox::OutboxRow,
    ) -> Result<Option<Vec<i64>>, MetaError> {
        if outbox.session_id != session_id {
            return Err(MetaError::Serialization(
                "event/outbox session ids do not match".into(),
            ));
        }
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let inserted = sqlx::query(
            r#"
            INSERT INTO session_outbox
                (prompt_id, session_id, kind, payload, created_at, not_before)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (prompt_id) DO NOTHING
            RETURNING prompt_id
            "#,
        )
        .bind(&outbox.prompt_id)
        .bind(session_id.as_uuid())
        .bind(outbox.kind.as_str())
        .bind(&outbox.payload)
        .bind(outbox.created_at)
        .bind(outbox.not_before)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .is_some();

        if !inserted {
            // A retry of the SAME command is the designed no-op; a
            // prompt_id claimed by a DIFFERENT command — including the
            // same kind with different text/mode (review finding on
            // #993: payload must be part of the identity, as it is in
            // `append_session_event_and_outbox`) — is a Conflict, never
            // a silent drop.
            let existing = sqlx::query(
                "SELECT session_id, kind, payload FROM session_outbox WHERE prompt_id = $1",
            )
            .bind(&outbox.prompt_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?
            .ok_or_else(|| {
                MetaError::Conflict(format!(
                    "outbox id {} disappeared during accept",
                    outbox.prompt_id
                ))
            })?;
            let existing_session: uuid::Uuid = existing.try_get("session_id").map_err(db_err)?;
            let existing_kind: String = existing.try_get("kind").map_err(db_err)?;
            let existing_payload: serde_json::Value =
                existing.try_get("payload").map_err(db_err)?;
            if existing_session != session_id.as_uuid()
                || existing_kind != outbox.kind.as_str()
                || existing_payload != outbox.payload
            {
                return Err(MetaError::Conflict(format!(
                    "outbox id {} belongs to another command",
                    outbox.prompt_id
                )));
            }
            tx.rollback().await.map_err(db_err)?;
            return Ok(None);
        }

        let mut idxs = Vec::with_capacity(events.len());
        for (kind, payload) in events {
            let event = sqlx::query(
                r#"
                WITH next AS (
                    UPDATE sessions
                       SET next_event_idx = next_event_idx + 1,
                           updated_at = $4,
                           last_event_at = $4
                     WHERE id = $1
                 RETURNING next_event_idx - 1 AS allocated_idx, recovery_epoch
                ),
                inserted AS (
                    INSERT INTO session_events (session_id, idx, kind, payload, recovery_epoch, created_at)
                    SELECT $1, allocated_idx, $2, $3, recovery_epoch, $4 FROM next
                    RETURNING idx
                )
                SELECT idx FROM inserted
                "#,
            )
            .bind(session_id.as_uuid())
            .bind(kind)
            .bind(payload)
            .bind(self.clock.now_utc())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?
            .ok_or(MetaError::NotFound)?;
            idxs.push(event.try_get::<i64, _>("idx").map_err(db_err)?);
        }

        // Notifications fire at commit: replicas see the events and the
        // outbox row together, so delivery can never observe the row
        // without its echo (the ADR 0052 echo-before-run_started
        // ordering, now transactional instead of sequenced).
        for idx in &idxs {
            sqlx::query("SELECT pg_notify('session_events', $1)")
                .bind(
                    serde_json::json!({ "session_id": session_id.to_string(), "idx": idx })
                        .to_string(),
                )
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
        }
        sqlx::query("SELECT pg_notify('session_outbox', $1)")
            .bind(session_id.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(Some(idxs))
    }

    async fn outbox_due_sessions(&self) -> Result<Vec<SessionId>, MetaError> {
        let rows = sqlx::query(
            "SELECT DISTINCT session_id FROM session_outbox
             WHERE acked_at IS NULL AND not_before <= $1",
        )
        .bind(self.clock.now_utc())
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.into_iter()
            .map(|r| {
                let id: uuid::Uuid = r.try_get(0).map_err(db_err)?;
                Ok(SessionId::from(id))
            })
            .collect()
    }

    async fn outbox_next_due(
        &self,
        session_id: SessionId,
    ) -> Result<Option<engram_core::types::outbox::OutboxRow>, MetaError> {
        let row = sqlx::query(
            "SELECT prompt_id, session_id, kind, payload, created_at, attempts,
                    not_before, delivered_at, acked_at
             FROM session_outbox
             WHERE session_id = $1 AND acked_at IS NULL AND not_before <= $2
             ORDER BY created_at ASC
             LIMIT 1",
        )
        .bind(session_id.as_uuid())
        .bind(self.clock.now_utc())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| outbox_row_from_pg(&r)).transpose()
    }

    async fn outbox_mark_delivered(
        &self,
        prompt_id: &str,
        ack_timeout: std::time::Duration,
    ) -> Result<(), MetaError> {
        sqlx::query(
            "UPDATE session_outbox
             SET delivered_at = $3,
                 attempts = attempts + 1,
                 not_before = $3 + make_interval(secs => $2)
             WHERE prompt_id = $1 AND acked_at IS NULL",
        )
        .bind(prompt_id)
        .bind(ack_timeout.as_secs_f64())
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn outbox_defer(
        &self,
        prompt_id: &str,
        delay: std::time::Duration,
    ) -> Result<(), MetaError> {
        // `attempts` counts every delivery *try*, deferred or delivered —
        // `failure_backoff(attempts)` only grows if defers bump it too. (It
        // used to bump only on mark_delivered, so a row failing before the
        // forward — e.g. ensure_active erroring — retried at the floor
        // backoff forever and read as attempts=0 in every investigation.)
        sqlx::query(
            "UPDATE session_outbox
             SET not_before = $3 + make_interval(secs => $2),
                 attempts = attempts + 1
             WHERE prompt_id = $1 AND acked_at IS NULL",
        )
        .bind(prompt_id)
        .bind(delay.as_secs_f64())
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn outbox_make_due(&self, session_id: SessionId) -> Result<u64, MetaError> {
        // No `attempts` bump: this cancels a provably-pointless wait
        // (e.g. an ACK_TIMEOUT armed by a forward into a dead harness
        // link, prod 7eddce62); it is not a delivery try, and a bump
        // would inflate `failure_backoff` for the very retry being
        // made prompt. The `not_before > $2` predicate makes the call
        // idempotent across repeated heartbeats and never touches a
        // row already due.
        let res = sqlx::query(
            "UPDATE session_outbox SET not_before = $2
             WHERE session_id = $1 AND acked_at IS NULL AND not_before > $2",
        )
        .bind(session_id.as_uuid())
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected())
    }

    async fn outbox_ack(&self, prompt_id: &str) -> Result<bool, MetaError> {
        let res = sqlx::query(
            "UPDATE session_outbox SET acked_at = $2
             WHERE prompt_id = $1 AND acked_at IS NULL",
        )
        .bind(prompt_id)
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
    }

    async fn outbox_update_prompt_text(
        &self,
        prompt_id: &str,
        text: &str,
    ) -> Result<bool, MetaError> {
        let res = sqlx::query(
            "UPDATE session_outbox
             SET payload = jsonb_set(payload, '{text}', to_jsonb($2::text))
             WHERE prompt_id = $1 AND kind = 'prompt'
               AND delivered_at IS NULL AND acked_at IS NULL",
        )
        .bind(prompt_id)
        .bind(text)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
    }

    async fn outbox_delete_undelivered(&self, prompt_id: &str) -> Result<bool, MetaError> {
        let res = sqlx::query(
            "DELETE FROM session_outbox
             WHERE prompt_id = $1 AND delivered_at IS NULL AND acked_at IS NULL",
        )
        .bind(prompt_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
    }

    async fn mint_binding_epoch(&self, id: SessionId) -> Result<u64, MetaError> {
        // ADR 0067: one atomic bump; the RETURNING value is the epoch
        // the caller stamps into the AgentSpec + bind RPC. Monotonic
        // per session by construction (single row, single counter).
        let row =
            sqlx::query("UPDATE sessions SET binding_epoch = binding_epoch + 1 WHERE id = $1 RETURNING binding_epoch")
                .bind(id.as_uuid())
                .fetch_optional(&self.pool)
                .await
                .map_err(db_err)?;
        let row = row.ok_or(MetaError::NotFound)?;
        let epoch: i64 = row.try_get(0).map_err(db_err)?;
        Ok(epoch as u64)
    }

    async fn current_binding_epoch(&self, id: SessionId) -> Result<u64, MetaError> {
        let row = sqlx::query("SELECT binding_epoch FROM sessions WHERE id = $1")
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        let row = row.ok_or(MetaError::NotFound)?;
        let epoch: i64 = row.try_get(0).map_err(db_err)?;
        Ok(epoch as u64)
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
        let now = self.clock.now_utc();
        let n = if sandbox_id.is_some() {
            sqlx::query(
                "UPDATE sessions SET sandbox_id = $2, missing_strikes = 0, updated_at = $3 \
                 WHERE id = $1",
            )
            .bind(id.as_uuid())
            .bind(sandbox_id.map(|s| s.as_uuid()))
            .bind(now)
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
                        updated_at                 = $2
                  WHERE id = $1",
            )
            .bind(id.as_uuid())
            .bind(now)
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
        let now = self.clock.now_utc();

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
                "UPDATE sessions SET sandbox_id = $2, missing_strikes = 0, updated_at = $3 \
                 WHERE id = $1",
            )
            .bind(id.as_uuid())
            .bind(sandbox_id.map(|s| s.as_uuid()))
            .bind(now)
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
                        updated_at                 = $2
                  WHERE id = $1",
            )
            .bind(id.as_uuid())
            .bind(now)
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
            UPDATE sessions SET host_id = $2, updated_at = $6
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
        .bind(self.clock.now_utc())
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
            UPDATE sessions SET host_id = $2, sandbox_id = $3, missing_strikes = 0, updated_at = $7
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
        .bind(self.clock.now_utc())
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
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
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
                updated_at              = $13
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
        .bind(self.clock.now_utc())
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
                   util_committed_swap_mib,
                   ready_images, current_bundles, sandbox_bundles,
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
        let n = sqlx::query(r#"UPDATE hosts SET status = $2, updated_at = $3 WHERE id = $1"#)
            .bind(id.as_uuid())
            .bind(status.as_str())
            .bind(self.clock.now_utc())
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
        // (ready_images / current_bundles / total_vcpus). `cordoned` is
        // deliberately absent: it is coordinator-owned and only
        // `set_host_cordoned` writes it. (ADR 0078 retired the dead
        // `local_snapshots` mirror.)
        let ready_images = serde_json::to_value(&hb.ready_images)
            .map_err(|e| MetaError::Serialization(e.to_string()))?;
        let current_bundles = serde_json::to_value(&hb.current_bundles)
            .map_err(|e| MetaError::Serialization(e.to_string()))?;
        // ADR 0035 amendment D2: per-running-sandbox aux attachments (migration 0113).
        let sandbox_bundles = serde_json::to_value(&hb.sandbox_bundles)
            .map_err(|e| MetaError::Serialization(e.to_string()))?;
        // ADR 0068: this tick's re-probed capability vector.
        let capabilities = serde_json::to_value(&hb.capabilities)
            .map_err(|e| MetaError::Serialization(e.to_string()))?;
        let placement_changed = sqlx::query_scalar::<_, bool>(
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
            r#"WITH previous AS MATERIALIZED (
                    SELECT status, allocatable_mib, ready_images, total_vcpus,
                           wire_version, capabilities
                      FROM hosts
                     WHERE id = $1
                     FOR UPDATE
                )
                UPDATE hosts
                  SET status = CASE WHEN hosts.status = 'dead' THEN 'dead' ELSE $2 END,
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
                      current_bundles = $13,
                      total_vcpus = $14,
                      wire_version = $15,
                      util_base_shm_mib = $16,
                      util_parked_pss_mib = $17,
                      util_running_pss_mib = $18,
                      capabilities = $19,
                      stages_images = $20,
                      last_heartbeat_at = $21,
                      util_committed_swap_mib = $22,
                      sandbox_bundles = $23,
                      updated_at = $21
                 FROM previous
                WHERE hosts.id = $1
                RETURNING previous.status <> 'dead' AND (
                    previous.status IS DISTINCT FROM
                        CASE WHEN previous.status = 'dead' THEN 'dead' ELSE $2 END
                    OR (COALESCE(previous.allocatable_mib, 0) = 0 AND $11::bigint > 0)
                    OR previous.ready_images IS DISTINCT FROM $12::jsonb
                    OR previous.total_vcpus IS DISTINCT FROM $14::int
                    OR previous.wire_version IS DISTINCT FROM $15::int
                    OR previous.capabilities IS DISTINCT FROM $19::jsonb
                )"#,
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
        .bind(self.clock.now_utc())
        .bind(hb.utilization.committed_swap_mib as i64)
        .bind(sandbox_bundles)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        let Some(placement_changed) = placement_changed else {
            return Err(MetaError::NotFound);
        };
        if placement_changed {
            // Registration wakes the queue before the host has its real
            // scheduling vector. Wake again when the first heartbeat supplies
            // capacity or when a later heartbeat changes an eligibility axis
            // such as image readiness. Steady heartbeats stay silent.
            self.notify_placement_changed("host_schedulability_changed")
                .await;
        }
        Ok(())
    }

    async fn set_host_cordoned(&self, id: HostId, cordoned: bool) -> Result<(), MetaError> {
        // ADR 0047: the coordinator-owned cordon bit. Heartbeats never
        // write this column, so the flip sticks until explicit uncordon.
        let n = sqlx::query(r#"UPDATE hosts SET cordoned = $2, updated_at = $3 WHERE id = $1"#)
            .bind(id.as_uuid())
            .bind(cordoned)
            .bind(self.clock.now_utc())
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
                   util_committed_swap_mib,
                   ready_images, current_bundles, sandbox_bundles,
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
                      AND last_heartbeat_at < $2 - make_interval(secs => $1::bigint))
                  OR last_heartbeat_at < $2 - make_interval(secs => $1::bigint * 10)
               )
            "#,
        )
        .bind(threshold_secs as i64)
        .bind(self.clock.now_utc())
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
        let now = self.clock.now_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        sqlx::query(r#"UPDATE hosts SET status = 'dead', updated_at = $2 WHERE id = $1"#)
            .bind(host_id.as_uuid())
            .bind(now)
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
                   last_active_at = $2
              FROM prior p
             WHERE s.id = p.id
            RETURNING s.id, p.prev_status
            "#,
        )
        .bind(host_id.as_uuid())
        .bind(now)
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

    async fn touch_session_activity(&self, session_id: SessionId) -> Result<(), MetaError> {
        sqlx::query("UPDATE sessions SET last_active_at = $2 WHERE id = $1")
            .bind(session_id.as_uuid())
            .bind(self.clock.now_utc())
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn host_status(&self, host_id: HostId) -> Result<Option<HostStatus>, MetaError> {
        let raw: Option<String> = sqlx::query_scalar("SELECT status FROM hosts WHERE id = $1")
            .bind(host_id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        raw.map(|s| row::parse_host_status(&s)).transpose()
    }

    async fn try_acquire_dead_host_lease(
        &self,
        host_id: HostId,
        claimant: &str,
        stale_after: std::time::Duration,
    ) -> Result<bool, MetaError> {
        let now = self.clock.now_utc();
        let stale_cutoff = now
            - chrono::Duration::from_std(stale_after)
                .unwrap_or_else(|_| chrono::Duration::seconds(180));
        // Free row: insert wins. Held row: the DO UPDATE fires only when
        // the incumbent's claim has gone stale (crash takeover); a live
        // incumbent means no row comes back and we lost the race.
        let won: Option<String> = sqlx::query_scalar(
            "INSERT INTO dead_host_inflight (host_id, claimed_by, claimed_at)
             VALUES ($1, $2, $3)
             ON CONFLICT (host_id) DO UPDATE
                 SET claimed_by = EXCLUDED.claimed_by, claimed_at = EXCLUDED.claimed_at
                 WHERE dead_host_inflight.claimed_at < $4
             RETURNING claimed_by",
        )
        .bind(host_id.as_uuid())
        .bind(claimant)
        .bind(now)
        .bind(stale_cutoff)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(won.is_some())
    }

    async fn release_dead_host_lease(
        &self,
        host_id: HostId,
        claimant: &str,
    ) -> Result<(), MetaError> {
        sqlx::query("DELETE FROM dead_host_inflight WHERE host_id = $1 AND claimed_by = $2")
            .bind(host_id.as_uuid())
            .bind(claimant)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn notify_host_dead(&self, host_id: HostId) -> Result<(), MetaError> {
        sqlx::query("SELECT pg_notify('host_dead', $1)")
            .bind(host_id.to_string())
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn put_session_runtime_spec(
        &self,
        session_id: SessionId,
        spec: &engram_core::types::runtime_spec::RuntimeSpec,
    ) -> Result<(), MetaError> {
        let json = serde_json::to_value(spec)
            .map_err(|e| MetaError::Serialization(format!("runtime_spec encode: {e}")))?;
        sqlx::query(
            "INSERT INTO session_runtime_specs (session_id, spec, updated_at)
             VALUES ($1, $2, $3)
             ON CONFLICT (session_id) DO UPDATE
               SET spec = EXCLUDED.spec, updated_at = $3",
        )
        .bind(session_id.as_uuid())
        .bind(json)
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_session_runtime_spec(
        &self,
        session_id: SessionId,
    ) -> Result<Option<engram_core::types::runtime_spec::RuntimeSpec>, MetaError> {
        let row = sqlx::query("SELECT spec FROM session_runtime_specs WHERE session_id = $1")
            .bind(session_id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        match row {
            Some(r) => {
                let json: serde_json::Value = r.try_get(0).map_err(db_err)?;
                let spec = serde_json::from_value(json)
                    .map_err(|e| MetaError::Serialization(format!("runtime_spec decode: {e}")))?;
                Ok(Some(spec))
            }
            None => Ok(None),
        }
    }

    async fn record_snapshot(&self, snap: SnapshotRecord) -> Result<bool, MetaError> {
        Ok(self
            .record_snapshot_guarded(snap, None)
            .await?
            .expect("unfenced record_snapshot always writes a row"))
    }

    /// ADR 0079 (re-review findings #3/#4): record a snapshot row ONLY
    /// while the session's `current_epoch` still equals `epoch`. `Ok(false)`
    /// means a successor op re-claimed the session (the epoch moved) and the
    /// row was NOT written — the fenced-out op executor must stop, never
    /// commit the capture. PG is the authority; this closes the window the
    /// host-side per-session epoch high-water leaves open between a PG
    /// reclaim and the successor's first fenced host RPC (so the proactive
    /// host-epoch-advance-at-reclaim, ADR 0079 deferral #1c, stays a pure
    /// optimization rather than a correctness requirement).
    async fn fenced_record_snapshot(
        &self,
        snap: SnapshotRecord,
        epoch: i64,
    ) -> Result<bool, MetaError> {
        Ok(self
            .record_snapshot_guarded(snap, Some(epoch))
            .await?
            .is_some())
    }

    async fn durable_head_snapshot(
        &self,
        session_id: SessionId,
    ) -> Result<Option<engram_core::types::SnapshotId>, MetaError> {
        let row = sqlx::query("SELECT durable_head_snapshot_id FROM sessions WHERE id = $1")
            .bind(session_id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        let row = row.ok_or(MetaError::NotFound)?;
        let id: Option<uuid::Uuid> = row.try_get(0).map_err(db_err)?;
        Ok(id.map(engram_core::types::SnapshotId::from))
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
              AND s.created_at < $2 - $1::interval
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
        .bind(self.clock.now_utc())
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
              AND s.created_at < $2 - $1::interval
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
        .bind(self.clock.now_utc())
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

    async fn session_exec_event_at(
        &self,
        session_id: SessionId,
        exec_id: &str,
        kind: ExecLifecycleEventKind,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>, MetaError> {
        // The event's OWN `at` stamp, not the row's `created_at`: the
        // exec_started stamp is the attach time, while its row lands only at
        // the first delivered frame — for a silent command that is the Exit
        // itself, so `created_at` would collapse wall_ms to ~0. Lifecycle
        // rows are written exclusively by the coordinator with a valid
        // RFC3339 `at`, so the cast never sees garbage.
        // MIN: the first recorded occurrence is the authoritative one — a
        // residual concurrent-attach duplicate must not move the timestamp.
        // Live timeline only: an ADR-0028 rewind tombstones exec rows AND
        // rewinds the guest journal, so a replayed step legitimately re-runs
        // the ticket — its fresh lifecycle rows must not be suppressed by
        // the tombstoned past.
        sqlx::query_scalar(
            r#"
            SELECT MIN((payload->>'at')::timestamptz)
              FROM session_events
             WHERE session_id = $1
               AND kind = $2
               AND payload->>'exec_id' = $3
               AND rewound_at IS NULL
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(kind.kind_str())
        .bind(exec_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)
    }

    async fn session_exec_output_high_water(
        &self,
        session_id: SessionId,
        exec_id: &str,
        stream: ExecOutputStream,
    ) -> Result<u64, MetaError> {
        // Unstamped (pre-ADR-0103) rows yield NULL from ->> and MAX skips
        // NULLs, so they are ignored by construction. Live timeline only:
        // rewound output rows must not hold the mark up — the re-run's
        // output must be re-recorded.
        let max: Option<i64> = sqlx::query_scalar(
            r#"
            SELECT MAX((payload->>'bytes_end')::bigint)
              FROM session_events
             WHERE session_id = $1
               AND kind = $2
               AND payload->>'exec_id' = $3
               AND rewound_at IS NULL
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(stream.kind_str())
        .bind(exec_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(max.unwrap_or(0).max(0) as u64)
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
                       updated_at = $4,
                       -- Track A: honest activity clock, bumped on every
                       -- event append (unlike last_active_at, which only
                       -- moves on state transitions).
                       last_event_at = $4
                 WHERE id = $1
             RETURNING next_event_idx - 1 AS allocated_idx, recovery_epoch
            ),
            inserted AS (
                INSERT INTO session_events (session_id, idx, kind, payload, recovery_epoch, created_at)
                SELECT $1, allocated_idx, $2, $3, recovery_epoch, $4 FROM next
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
        .bind(self.clock.now_utc())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?
        .ok_or(MetaError::NotFound)?;
        let idx: i64 = sqlx::Row::try_get(&row, "idx").map_err(db_err)?;
        Ok(idx)
    }

    async fn append_session_event_fenced(
        &self,
        session_id: SessionId,
        epoch: i64,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<Option<i64>, MetaError> {
        // Identical to `append_session_event` but the idx-allocation
        // UPDATE carries `AND current_epoch = $4`. 0 rows (fenced) ⇒ the
        // whole statement returns nothing → `Ok(None)`; the fenced-out
        // predecessor's event never lands after the successor's.
        let row = sqlx::query(
            r#"
            WITH next AS (
                UPDATE sessions
                   SET next_event_idx = next_event_idx + 1,
                       updated_at = $5,
                       last_event_at = $5
                 WHERE id = $1 AND current_epoch = $4
             RETURNING next_event_idx - 1 AS allocated_idx, recovery_epoch
            ),
            inserted AS (
                INSERT INTO session_events (session_id, idx, kind, payload, recovery_epoch, created_at)
                SELECT $1, allocated_idx, $2, $3, recovery_epoch, $5 FROM next
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
        .bind(epoch)
        .bind(self.clock.now_utc())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        match row {
            Some(row) => Ok(Some(sqlx::Row::try_get(&row, "idx").map_err(db_err)?)),
            None => Ok(None),
        }
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

    async fn list_session_events_window(
        &self,
        session_id: SessionId,
        cursor: EventCursor,
        limit: i64,
        kinds: &[String],
        tool_names: &[String],
    ) -> Result<Vec<PersistedEvent>, MetaError> {
        // THE one event-log query builder. `_since` and `_tail` are the
        // trait's thin wrappers over it, so a change to the row shape or
        // to the rewind semantics has exactly one place to land.
        //
        // ADR 0028 A.log: read ALL events (incl. tombstoned), each
        // carrying its `recovery_epoch` + `rewound_at`. The transcript
        // renders rewound rows collapsed/greyed and segments by epoch —
        // honest history, not a silent deletion.
        //
        // Both filters go INTO the query: a caller that wants 200 `Edit`
        // calls out of a 50k-event session must read 200 rows from
        // Postgres, not 50k rows plus a client-side loop.
        const COLS: &str = "idx, kind, payload, created_at, recovery_epoch, rewound_at";

        // Placeholders: $1 session, $2 anchor, then each filter it uses,
        // then the limit. The binds below follow the SAME order.
        let mut next_placeholder = 3;
        let mut predicates = String::new();
        if !kinds.is_empty() {
            predicates.push_str(&format!(" AND kind = ANY(${next_placeholder})"));
            next_placeholder += 1;
        }
        if !tool_names.is_empty() {
            // The kind→field map comes from engram-core, so the SQL and
            // the sim read one table. Every part is a compile-time
            // constant — no caller string reaches the query text.
            let mut case = String::from("CASE kind");
            for (kind, field) in engram_core::types::event::TOOL_NAME_FIELDS {
                case.push_str(&format!(" WHEN '{kind}' THEN payload->>'{field}'"));
            }
            case.push_str(" ELSE NULL END");
            // `IS NULL` passes the row: a kind that carries no tool name
            // is never removed by a tool-name filter (trait contract).
            predicates.push_str(&format!(
                " AND ({case} IS NULL OR {case} = ANY(${next_placeholder}))"
            ));
            next_placeholder += 1;
        }
        let limit_placeholder = next_placeholder;

        let (anchor, sql) = match cursor {
            EventCursor::After(n) => (
                n,
                format!(
                    "SELECT {COLS} \
                       FROM session_events \
                      WHERE session_id = $1 AND idx > $2{predicates} \
                      ORDER BY idx \
                      LIMIT ${limit_placeholder}"
                ),
            ),
            // Newest-first inside, re-ascended outside, so the backward
            // page arrives in the same order as a forward one (a plain
            // ORDER BY idx DESC would render the transcript backwards).
            EventCursor::Before(n) => (
                n,
                format!(
                    "SELECT {COLS} \
                       FROM ( \
                         SELECT {COLS} \
                           FROM session_events \
                          WHERE session_id = $1 AND idx < $2{predicates} \
                          ORDER BY idx DESC \
                          LIMIT ${limit_placeholder} \
                       ) newest \
                      ORDER BY idx"
                ),
            ),
        };

        let mut q = sqlx::query(&sql).bind(session_id.as_uuid()).bind(anchor);
        if !kinds.is_empty() {
            q = q.bind(kinds.to_vec());
        }
        if !tool_names.is_empty() {
            q = q.bind(tool_names.to_vec());
        }
        // A non-positive limit is an empty page, never "unlimited" (the
        // trait contract) — and PG rejects a negative LIMIT outright.
        let rows = q
            .bind(limit.max(0))
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
        // ADR 0098 D3: `now` is now the coordinator clock (a bound
        // parameter), and `created_at` is the same coordinator clock
        // stamped at append time (`append_session_event` binds it too), so
        // both ends read one injected clock — the single-clock property PR
        // #556 wanted, now sourced from `services.clock` instead of PG.
        // (Cross-replica reads still see whichever replica stamped the
        // append; production SystemClocks are NTP-bounded, and this is a
        // telemetry sample, not a decision gate.)
        let row = sqlx::query(
            r#"
            SELECT EXTRACT(EPOCH FROM ($3::timestamptz - created_at))::float8 AS secs_ago
              FROM session_events
             WHERE session_id = $1 AND kind = 'prompt_received' AND payload->>'prompt_id' = $2
             ORDER BY idx DESC
             LIMIT 1
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(prompt_id)
        .bind(self.clock.now_utc())
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
        let now = self.clock.now_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;

        // Surviving side-effects: outside-world actions in the
        // rolled-back span that the rewind CANNOT undo. We surface
        // them rather than hide them (the deliberate at-least-once
        // posture). Detect the kinds that touched the world.
        // `file_shared` left this detector when it left the rewindable
        // set (user input survives the rewind, so the event itself
        // stays visible — a "still exists" note would be redundant).
        let side_effect_rows = sqlx::query(
            r#"
            SELECT kind, payload FROM session_events
             WHERE session_id = $1 AND idx > $2 AND rewound_at IS NULL
               AND kind = 'integration_asset' AND payload->>'surface' = 'asset'
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
                    _ => return None,
                })
            })
            .collect();

        // Tombstone the rolled-back span (audit-preserving) and count it.
        //
        // The predicate is POSITIVE: it names the guest-history kinds
        // the rewind MAY touch — the closed set the harness sink and
        // the exec API produce, whose truth lives in guest state the
        // checkpoint does not cover. Everything else survives BY
        // DEFAULT: coordinator facts (status_changed, snapshot_taken,
        // evicted, resumed, resume_started, recovered_from_checkpoint,
        // durability_rollback — issue #529), stable harness waiting
        // markers (harness_idle / harness_parked — ADR 0091/0089),
        // user intent (prompt_received — issue #527;
        // harness_mode_changed — ADR 0107), user INPUT (the
        // agent_message user echo, tool_result_submitted, file_shared
        // — prod 2026-08-03: a prompt sent to an idle session raced
        // the un-park rewind and was tombstoned, reading as a
        // "checkpoint/restore crash"), and any kind added in the
        // future. The old NOT IN list had the opposite default — every
        // new non-guest kind was silently rewindable until someone
        // remembered to exclude it, and #529, ADR 0091, #527, ADR 0107
        // and the 2026-08-03 incident were each that default firing.
        //
        // `agent_message` is the one kind with mixed provenance: the
        // harness's assistant/system messages are guest history; the
        // user echo is input. A row with no role survives (uncertainty
        // never destroys). Keep this predicate in lockstep with
        // `SimMetadataStore::rewind_session_to_cursor` (ADR 0098 D4:
        // any change here needs the conformance case extended in the
        // same PR).
        let tombstoned = sqlx::query(
            r#"
            UPDATE session_events
               SET rewound_at = $3
             WHERE session_id = $1 AND idx > $2 AND rewound_at IS NULL
               AND (
                   kind IN (
                       'run_started', 'run_completed', 'run_interrupted',
                       'tool_call_started', 'tool_call_completed', 'tool_call_requested',
                       'browser_activity',
                       'prompt_queued', 'prompt_edited', 'prompt_dequeued', 'prompt_steered',
                       'file_changed', 'title_suggested', 'integration_asset',
                       'exec_started', 'exec_completed', 'stdout', 'stderr'
                   )
                   OR (kind = 'agent_message' AND payload->>'role' <> 'user')
               )
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(events_cursor)
        .bind(now)
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
            "UPDATE sessions SET recovery_epoch = recovery_epoch + 1, updated_at = $2 \
             WHERE id = $1 RETURNING recovery_epoch",
        )
        .bind(session_id.as_uuid())
        .bind(now)
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
        file_name: Option<&str>,
    ) -> Result<(), MetaError> {
        sqlx::query(
            r#"
            INSERT INTO artifacts (id, session_id, blob_key, media_type, size_bytes, caption, file_name)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(id)
        .bind(session_id.as_uuid())
        .bind(blob_key)
        .bind(media_type)
        .bind(size_bytes)
        .bind(caption)
        .bind(file_name)
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
            SELECT id, blob_key, media_type, size_bytes, caption, file_name, created_at
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
                updated_at  = $6
            "#,
        )
        .bind(cred.id)
        .bind(&cred.registry_host)
        .bind(auth_kind)
        .bind(auth_config)
        .bind(cred.created_at)
        .bind(self.clock.now_utc())
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
                (id, image_uri, image_config, oci_defaults, manifest_digest,
                 disk_manifest_id, disk_manifest_version, base_snapshot_id,
                 base_snapshot_disk_manifest_id, base_snapshot_disk_manifest_version,
                 base_snapshot_memory_manifest_id, base_snapshot_memory_manifest_version,
                 last_refreshed_at, created_at, updated_at, soft_deleted_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, NULL, NULL)
            ON CONFLICT (image_uri) DO UPDATE SET
                image_config          = EXCLUDED.image_config,
                oci_defaults          = EXCLUDED.oci_defaults,
                manifest_digest       = EXCLUDED.manifest_digest,
                disk_manifest_id      = EXCLUDED.disk_manifest_id,
                disk_manifest_version = EXCLUDED.disk_manifest_version,
                base_snapshot_id      = EXCLUDED.base_snapshot_id,
                base_snapshot_disk_manifest_id      = EXCLUDED.base_snapshot_disk_manifest_id,
                base_snapshot_disk_manifest_version = EXCLUDED.base_snapshot_disk_manifest_version,
                base_snapshot_memory_manifest_id      = EXCLUDED.base_snapshot_memory_manifest_id,
                base_snapshot_memory_manifest_version = EXCLUDED.base_snapshot_memory_manifest_version,
                last_refreshed_at     = EXCLUDED.last_refreshed_at,
                updated_at            = $15,
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
        .bind(sqlx::types::Json(&image.image_config))
        .bind(sqlx::types::Json(&image.oci_defaults))
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
        .bind(self.clock.now_utc())
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

    async fn update_enabled_image_config(
        &self,
        image_uri: &str,
        config: &engram_core::types::image::ImageConfig,
    ) -> Result<(), MetaError> {
        // ADR 0080 cheap-edit path: replace image_config in place on a
        // live row — no snapshot work, so callers must have gated out
        // capture-affecting diffs (resources/warm) before landing here.
        // Same transactional NOTIFY as upsert_enabled_image so every
        // replica's boot-bundle cache drops its copy immediately.
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let res = sqlx::query(
            "UPDATE enabled_images SET image_config = $2, updated_at = $3 \
             WHERE image_uri = $1 AND soft_deleted_at IS NULL",
        )
        .bind(image_uri)
        .bind(sqlx::types::Json(config))
        .bind(self.clock.now_utc())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(MetaError::NotFound);
        }
        sqlx::query("SELECT pg_notify('enabled_image_changed', $1)")
            .bind(image_uri)
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
            SELECT id, image_uri, image_config, oci_defaults, manifest_digest,
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
            SELECT id, image_uri, image_config, oci_defaults, manifest_digest,
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
            SELECT id, image_uri, image_config, oci_defaults, manifest_digest,
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
             SET soft_deleted_at = $2, updated_at = $2 \
             WHERE image_uri = $1",
        )
        .bind(image_uri)
        .bind(self.clock.now_utc())
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
        resources: &engram_core::types::image::ResourceHints,
    ) -> Result<Option<EnabledImage>, MetaError> {
        // Soft-deleted rows are deliberately INCLUDED: their base
        // snapshots remain GC-pinned and restorable, and content
        // equality is what makes the reuse sound — liveness of the
        // *row* is irrelevant to the snapshot's validity.
        //
        // ADR 0080: the config half of the reuse key is ONLY the
        // capture-affecting `resources` slice (JSONB containment on
        // `image_config->'resources'`, order-insensitive) — name/
        // description/env/workdir are applied per-session and don't
        // invalidate a snapshot. Warm images never reach this query
        // (caller-gated).
        let resources_json =
            serde_json::to_value(resources).map_err(|e| MetaError::Serialization(e.to_string()))?;
        let row = sqlx::query(
            r#"
            SELECT id, image_uri, image_config, oci_defaults, manifest_digest,
                   disk_manifest_id, disk_manifest_version, base_snapshot_id,
                   base_snapshot_disk_manifest_id, base_snapshot_disk_manifest_version,
                   base_snapshot_memory_manifest_id, base_snapshot_memory_manifest_version,
                   last_refreshed_at, created_at, updated_at, soft_deleted_at
              FROM enabled_images
             WHERE disk_manifest_id = $1
               AND disk_manifest_version = $2
               AND image_config->'resources' = $3::jsonb
               AND base_snapshot_id IS NOT NULL
             ORDER BY COALESCE(updated_at, created_at) DESC
             LIMIT 1
            "#,
        )
        .bind(disk_manifest.manifest_id)
        .bind(disk_manifest.version as i64)
        .bind(resources_json)
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
        image_config: &engram_core::types::image::ImageConfig,
    ) -> Result<EnableJob, MetaError> {
        self.create_or_get_enable_job_with_options(image_uri, manifest_digest, image_config, false)
            .await
    }

    async fn create_or_get_enable_job_with_options(
        &self,
        image_uri: &str,
        manifest_digest: Option<&str>,
        image_config: &engram_core::types::image::ImageConfig,
        force_recapture: bool,
    ) -> Result<EnableJob, MetaError> {
        // INSERT guarded by the partial unique index (one non-terminal
        // job per image_uri); on conflict fall through to SELECTing
        // the in-flight job. Re-POST = resume, never duplicate work.
        // The full `image_config` rides the job (ADR 0080): the scanner
        // captures under it (warm env refs resolved there, values never
        // stored) and stamps it onto the enabled_images row at ready.
        let inserted = sqlx::query(
            r#"
            INSERT INTO enable_jobs (id, image_uri, manifest_digest, image_config,
                                     force_recapture)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (image_uri) WHERE state NOT IN ('ready', 'failed')
            DO NOTHING
            RETURNING id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, image_config, force_recapture, capture_phase, warm_stage, warm_stage_started_at, warm_stages, materialize_stages, materialize_host_id, output_tail, prestage_hosts, created_at, updated_at
            "#,
        )
        .bind(self.entropy.uuid())
        .bind(image_uri)
        .bind(manifest_digest)
        .bind(sqlx::types::Json(image_config))
        .bind(force_recapture)
        // ADR 0084 (c): the capture placement reservation lives on
        // `capture_jobs` now — its budgets are stamped there at
        // `insert_capture_job` time (from the same `ImageConfig`
        // derivation), not on this enable-job row.
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        if let Some(row) = inserted {
            return row::enable_job_from_row(&row);
        }
        let existing = sqlx::query(
            r#"
            SELECT id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, image_config, force_recapture, capture_phase, warm_stage, warm_stage_started_at, warm_stages, materialize_stages, materialize_host_id, output_tail, prestage_hosts, created_at, updated_at
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
                    INSERT INTO enable_jobs (id, image_uri, manifest_digest, image_config,
                                             force_recapture)
                    VALUES ($1, $2, $3, $4, $5)
                    ON CONFLICT (image_uri) WHERE state NOT IN ('ready', 'failed')
                    DO NOTHING
                    RETURNING id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, image_config, force_recapture, capture_phase, warm_stage, warm_stage_started_at, warm_stages, materialize_stages, materialize_host_id, output_tail, prestage_hosts, created_at, updated_at
                    "#,
                )
                .bind(self.entropy.uuid())
                .bind(image_uri)
                .bind(manifest_digest)
                .bind(sqlx::types::Json(image_config))
                .bind(force_recapture)
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
            r#"SELECT id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, image_config, force_recapture, capture_phase, warm_stage, warm_stage_started_at, warm_stages, materialize_stages, materialize_host_id, output_tail, prestage_hosts, created_at, updated_at FROM enable_jobs WHERE id = $1"#,
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
            SELECT id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, image_config, force_recapture, capture_phase, warm_stage, warm_stage_started_at, warm_stages, materialize_stages, materialize_host_id, output_tail, prestage_hosts, created_at, updated_at
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
        //
        // ADR 0084 (c): no capture reservation to clear here anymore — the
        // reservation moved onto `capture_jobs` (released implicitly on a
        // terminal stage), so a lost enable-job lease no longer strands a
        // phantom capture reservation on this row.
        let rows = sqlx::query(
            r#"
            UPDATE enable_jobs
               SET claimed_by = $1, claimed_at = $4, updated_at = $4
             WHERE id IN (
                   SELECT id FROM enable_jobs
                    WHERE state NOT IN ('ready', 'failed')
                      AND (claimed_at IS NULL OR claimed_at < $4 - make_interval(secs => $2))
                    ORDER BY created_at
                    LIMIT $3
                      FOR UPDATE SKIP LOCKED
             )
            RETURNING id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, image_config, force_recapture, capture_phase, warm_stage, warm_stage_started_at, warm_stages, materialize_stages, materialize_host_id, output_tail, prestage_hosts, created_at, updated_at
            "#,
        )
        .bind(claimant)
        .bind(lease_secs as f64)
        .bind(limit as i64)
        .bind(self.clock.now_utc())
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
                   claimed_at = $5,
                   updated_at = $5
             WHERE id = $1 AND claimed_by = $2
            "#,
        )
        .bind(id)
        .bind(claimant)
        .bind(chunks_done as i32)
        .bind(chunks_total.map(|v| v as i32))
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(self.enable_job_fence_miss(id, claimant).await);
        }
        Ok(())
    }

    /// ADR 0080 phase 3b: persist one `MaterializeProgress` frame onto
    /// the job row. Post-3b the `materializing` stage runs HOST-side —
    /// there is no coordinator chunk counter anymore, so the honest
    /// operator surface is a rendered progress line in `output_tail`
    /// (the same column the capture phase's hook output rides). Fenced
    /// + claim-renewing exactly like the ADR 0084 P1b
    /// `mirror_capture_progress_to_enable_job` (UNFENCED, unlike this
    /// verb — see its own doc) does for the capture phase; the fenced,
    /// claim-renewing capture-progress verb this comment used to point
    /// at (`update_enable_job_capture_progress`) was DELETED as dead
    /// code in ADR 0084 P4.
    async fn update_enable_job_materialize_progress(
        &self,
        id: Uuid,
        claimant: &str,
        progress: &engram_core::types::MaterializeProgress,
        stages: &[engram_core::types::WarmStageRecord],
    ) -> Result<(), MetaError> {
        let line = match &progress.detail {
            Some(detail) => format!("materialize[{}] {detail}", progress.stage),
            None => format!("materialize[{}]", progress.stage),
        };
        let stages_json =
            serde_json::to_value(stages).map_err(|e| MetaError::Serialization(e.to_string()))?;
        // Chunk-stage frames carry window counts (i32 columns; a count
        // past i32::MAX would need a >32 PiB ext4 — clamp, don't wrap).
        let chunks_done: Option<i32> = progress.chunks_done.map(|v| v.min(i32::MAX as u64) as i32);
        let chunks_total: Option<i32> =
            progress.chunks_total.map(|v| v.min(i32::MAX as u64) as i32);
        let res = sqlx::query(
            r#"
            UPDATE enable_jobs
               SET output_tail = $3,
                   materialize_stages = $4,
                   chunks_done = COALESCE($5, chunks_done),
                   chunks_total = COALESCE($6, chunks_total),
                   claimed_at = $7,
                   updated_at = $7
             WHERE id = $1 AND claimed_by = $2
            "#,
        )
        .bind(id)
        .bind(claimant)
        .bind(&line)
        .bind(stages_json)
        .bind(chunks_done)
        .bind(chunks_total)
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(self.enable_job_fence_miss(id, claimant).await);
        }
        Ok(())
    }

    async fn set_enable_job_materialize_host(
        &self,
        id: Uuid,
        claimant: &str,
        host: HostId,
    ) -> Result<(), MetaError> {
        let res = sqlx::query(
            r#"
            UPDATE enable_jobs
               SET materialize_host_id = $3,
                   updated_at = $4
             WHERE id = $1 AND claimed_by = $2
            "#,
        )
        .bind(id)
        .bind(claimant)
        .bind(host.as_uuid())
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(self.enable_job_fence_miss(id, claimant).await);
        }
        Ok(())
    }

    async fn live_enable_work_by_host(
        &self,
        materialize_lease: std::time::Duration,
    ) -> Result<std::collections::HashMap<HostId, engram_core::types::LiveEnableWork>, MetaError>
    {
        let mut out: std::collections::HashMap<HostId, engram_core::types::LiveEnableWork> =
            std::collections::HashMap::new();
        // Live materializes: fresh-claimed `materializing` rows bound to a
        // host. The keepalive frames renew `claimed_at` (~every <=30 s), so a
        // dead stream ages out of this set within the lease window.
        let mat_rows: Vec<(Uuid, i64)> = sqlx::query_as(
            r#"
            SELECT materialize_host_id, COUNT(*)
              FROM enable_jobs
             WHERE state = 'materializing'
               AND materialize_host_id IS NOT NULL
               AND claimed_at IS NOT NULL
               AND claimed_at > $2 - make_interval(secs => $1)
             GROUP BY materialize_host_id
            "#,
        )
        .bind(materialize_lease.as_secs_f64())
        .bind(self.clock.now_utc())
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        for (host, n) in mat_rows {
            out.entry(HostId::from(host)).or_default().materializes = n.max(0) as u32;
        }
        // Live captures: non-terminal stages bound to a host (WAITING rows
        // carry NULL host_id). No freshness filter — the enable scanner's
        // stage deadlines redrive-or-fail a stuck row.
        let cap_rows: Vec<(Uuid, i64)> = sqlx::query_as(
            r#"
            SELECT host_id, COUNT(*)
              FROM capture_jobs
             WHERE stage NOT IN ('done','failed')
               AND host_id IS NOT NULL
             GROUP BY host_id
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        for (host, n) in cap_rows {
            out.entry(HostId::from(host)).or_default().captures = n.max(0) as u32;
        }
        Ok(out)
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
                   claimed_at = CASE WHEN $3 IN ('ready', 'failed') THEN NULL ELSE $4 END,
                   updated_at = $4
             WHERE id = $1 AND claimed_by = $2
            "#,
        )
        .bind(id)
        .bind(claimant)
        .bind(state.as_str())
        .bind(self.clock.now_utc())
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
                   -- ADR 0084 (c): no capture reservation to release here —
                   -- it lives on `capture_jobs` and drops out of every
                   -- reserved-SUM implicitly once that row goes terminal.
                   updated_at = $6
             WHERE id = $1 AND claimed_by = $2
            RETURNING attempts, state
            "#,
        )
        .bind(id)
        .bind(claimant)
        .bind(error)
        .bind(max_attempts as i32)
        .bind(force_terminal)
        .bind(self.clock.now_utc())
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
        // emitting one). Includes the `chunks_done`/`chunks_total`
        // counters — same live-progress class, same staleness bug.
        let row = sqlx::query(
            r#"
            UPDATE enable_jobs
               SET state = 'pending', attempts = 0, error = NULL,
                   claimed_by = NULL, claimed_at = NULL, updated_at = $2,
                   capture_phase = NULL, warm_stage = NULL,
                   warm_stage_started_at = NULL, warm_stages = NULL,
                   output_tail = NULL,
                   -- ADR 0084 (c): no capture reservation on this row
                   -- anymore; the retry's fresh `capture_jobs` row
                   -- re-reserves from scratch (and the DELETE below clears
                   -- the old terminal capture row).
                   -- Also reset the chunk-progress counters (same class as
                   -- the warm/capture columns above): the UI renders these
                   -- as live progress too, and the progress checkpoint
                   -- writes chunks_done absolutely, so a stale value would
                   -- read as live until the retry's first chunk event.
                   chunks_done = 0, chunks_total = NULL
             WHERE id = $1 AND state = 'failed'
            RETURNING id, image_uri, manifest_digest, state, chunks_total, chunks_done, attempts, error, image_config, force_recapture, capture_phase, warm_stage, warm_stage_started_at, warm_stages, materialize_stages, materialize_host_id, output_tail, prestage_hosts, created_at, updated_at
            "#,
        )
        .bind(id)
        .bind(self.clock.now_utc())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        match row {
            Some(r) => {
                // ADR 0084 P1b: a prior (failed) attempt's TERMINAL
                // `capture_jobs` row must not survive a retry — the
                // scanner's `latest_capture_job_for_enable` read is
                // "newest row for this enable job, terminal or not" (so
                // it can observe a `done`/`failed` outcome), which would
                // otherwise keep re-surfacing the OLD exhausted failure
                // forever and wedge the retry. Only terminal rows: a
                // live (non-terminal) row can't coexist with `state =
                // 'failed'` above (retry only applies once the capture
                // pipeline itself gave up), so this is a no-op in the
                // normal case and a safety net against any stale leftover.
                if let Err(e) = sqlx::query(
                    "DELETE FROM capture_jobs WHERE enable_job_id = $1 AND stage IN ('done', 'failed')",
                )
                .bind(id)
                .execute(&self.pool)
                .await
                {
                    tracing::warn!(enable_job_id = %id, error = %e, "retry_enable_job: failed to clear stale terminal capture_jobs row (non-fatal; the scanner may re-observe the old outcome)");
                }
                row::enable_job_from_row(&r)
            }
            None => match self.get_enable_job(id).await? {
                Some(job) => Err(MetaError::Conflict(format!(
                    "enable job {id} is `{}`, not `failed`; only failed jobs can be retried",
                    job.state.as_str()
                ))),
                None => Err(MetaError::NotFound),
            },
        }
    }

    /// ADR 0084 P1b: release the claim without touching state/attempts —
    /// the watch-only exit for a `Capturing` job whose `capture_jobs` row
    /// is still in flight. See the trait doc for why this exists
    /// alongside `claim_enable_jobs`'s lease-expiry path.
    async fn release_enable_job_claim(&self, id: Uuid, claimant: &str) -> Result<bool, MetaError> {
        let res = sqlx::query(
            r#"
            UPDATE enable_jobs
               SET claimed_by = NULL, claimed_at = NULL, updated_at = $3
             WHERE id = $1 AND claimed_by = $2
            "#,
        )
        .bind(id)
        .bind(claimant)
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
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
                   claimed_at = $4,
                   updated_at = $4
             WHERE id = $1 AND claimed_by = $2
            "#,
        )
        .bind(id)
        .bind(claimant)
        .bind(prestage_ref)
        .bind(self.clock.now_utc())
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
                   claimed_at = $4,
                   updated_at = $4
             WHERE id = $1 AND claimed_by = $2
            "#,
        )
        .bind(id)
        .bind(claimant)
        .bind(outcomes)
        .bind(self.clock.now_utc())
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

    // ---- capture jobs (ADR 0084) ----

    async fn insert_capture_job(&self, row: NewCaptureJob) -> Result<CaptureJobRow, MetaError> {
        // Insert-or-get, exactly like `create_or_get_enable_job`: guarded
        // by the `capture_jobs_active_enable` partial unique index, so a
        // coordinator restart or a re-driven scanner tick resumes the
        // existing attempt instead of duplicating a capture VM.
        //
        // ADR 0084 (c): a fresh row is inserted WAITING — `host_id NULL`,
        // `waiting_since NOW()` — with its placement budgets stamped from
        // the image config (no zero-budget row can ever exist). No host is
        // chosen here: `place_capture_job` runs the atomic 2D fit as a
        // separate step (keeping the pick atomic with the host-row lock).
        const COLUMNS: &str = "id, enable_job_id, image_uri, manifest_digest, disk_manifest, \
            image_config, oci_defaults, host_id, mem_budget_mib, cpu_budget_vcpus, waiting_since, epoch, stage, stage_started_at, stage_progress, \
            last_progress_at, attempts, retryable, error, error_stage, fc_snapshot_version, \
            result_bincode, created_at, updated_at";
        // ADR 0098 D3: stamp waiting_since AND the columns that otherwise
        // default to PG `now()` (stage_started_at / last_progress_at —
        // both compared by `expire_capture_job_stages` — plus created_at /
        // updated_at) from the injected clock, so a fresh capture row is
        // fully coordinator-clock-stamped and time-controllable.
        let insert_sql = format!(
            r#"
            INSERT INTO capture_jobs (id, enable_job_id, image_uri, manifest_digest, disk_manifest, image_config, oci_defaults, mem_budget_mib, cpu_budget_vcpus, waiting_since, stage_started_at, last_progress_at, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $10, $10, $10, $10)
            ON CONFLICT (enable_job_id) WHERE stage NOT IN ('done', 'failed')
            DO NOTHING
            RETURNING {COLUMNS}
            "#
        );
        let select_sql = format!(
            r#"
            SELECT {COLUMNS}
              FROM capture_jobs
             WHERE enable_job_id = $1 AND stage NOT IN ('done', 'failed')
            "#
        );
        let inserted = sqlx::query(&insert_sql)
            .bind(self.entropy.uuid())
            .bind(row.enable_job_id)
            .bind(&row.image_uri)
            .bind(&row.manifest_digest)
            .bind(&row.disk_manifest)
            .bind(sqlx::types::Json(&row.image_config))
            .bind(sqlx::types::Json(&row.oci_defaults))
            .bind(row.mem_budget_mib)
            .bind(row.cpu_budget_vcpus)
            .bind(self.clock.now_utc())
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        if let Some(r) = inserted {
            return row::capture_job_from_row(&r);
        }
        let existing = sqlx::query(&select_sql)
            .bind(row.enable_job_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        match existing {
            Some(r) => row::capture_job_from_row(&r),
            // Raced with the active job reaching a terminal state between
            // INSERT and SELECT — retry the insert once, mirroring
            // `create_or_get_enable_job`.
            None => {
                let r = sqlx::query(&insert_sql)
                    .bind(self.entropy.uuid())
                    .bind(row.enable_job_id)
                    .bind(&row.image_uri)
                    .bind(&row.manifest_digest)
                    .bind(&row.disk_manifest)
                    .bind(sqlx::types::Json(&row.image_config))
                    .bind(sqlx::types::Json(&row.oci_defaults))
                    .bind(row.mem_budget_mib)
                    .bind(row.cpu_budget_vcpus)
                    .bind(self.clock.now_utc())
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(db_err)?
                    .ok_or_else(|| {
                        MetaError::Conflict(format!(
                            "capture job for enable job {} raced two creates; retry",
                            row.enable_job_id
                        ))
                    })?;
                row::capture_job_from_row(&r)
            }
        }
    }

    async fn get_capture_job(&self, id: CaptureJobId) -> Result<Option<CaptureJobRow>, MetaError> {
        let row = sqlx::query(
            r#"
            SELECT id, enable_job_id, image_uri, manifest_digest, disk_manifest, image_config,
                   oci_defaults, host_id, mem_budget_mib, cpu_budget_vcpus, waiting_since, epoch, stage, stage_started_at, stage_progress,
                   last_progress_at, attempts, retryable, error, error_stage, fc_snapshot_version,
                   result_bincode, created_at, updated_at
              FROM capture_jobs
             WHERE id = $1
            "#,
        )
        .bind(id.as_uuid())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| row::capture_job_from_row(&r)).transpose()
    }

    async fn latest_capture_job_for_enable(
        &self,
        enable_job_id: Uuid,
    ) -> Result<Option<CaptureJobRow>, MetaError> {
        let row = sqlx::query(
            r#"
            SELECT id, enable_job_id, image_uri, manifest_digest, disk_manifest, image_config,
                   oci_defaults, host_id, mem_budget_mib, cpu_budget_vcpus, waiting_since, epoch, stage, stage_started_at, stage_progress,
                   last_progress_at, attempts, retryable, error, error_stage, fc_snapshot_version,
                   result_bincode, created_at, updated_at
              FROM capture_jobs
             WHERE enable_job_id = $1
             ORDER BY created_at DESC
             LIMIT 1
            "#,
        )
        .bind(enable_job_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| row::capture_job_from_row(&r)).transpose()
    }

    async fn record_capture_job_report(
        &self,
        report: &CaptureJobReport,
    ) -> Result<bool, MetaError> {
        // ONE fenced write any replica can perform — no lease-holder
        // identity to lose (ADR 0084). `stage_started_at` only advances
        // when the stage actually changed; `stage_progress` is a direct
        // SET (a fresh report replaces the prior progress snapshot
        // wholesale, matching the streaming protocol's "full frame, not a
        // diff" shape); the terminal fields COALESCE so a terminal
        // report's re-advertisement (before the coord acks it) doesn't
        // need to resend them to stay a no-op — though in practice the
        // WHERE clause already fences off any write once `stage` is
        // `done`/`failed`.
        let stage_progress = report
            .progress
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| MetaError::Serialization(e.to_string()))?;
        let (stage, retryable, error, error_stage, result_bincode): (
            &str,
            Option<bool>,
            Option<&str>,
            Option<&str>,
            Option<&[u8]>,
        ) = match &report.terminal {
            Some(CaptureTerminalReport::Done { result_bincode }) => {
                ("done", None, None, None, Some(result_bincode.as_slice()))
            }
            Some(CaptureTerminalReport::Failed {
                error,
                error_stage,
                retryable,
            }) => (
                "failed",
                Some(*retryable),
                Some(error.as_str()),
                Some(error_stage.as_str()),
                None,
            ),
            None => (report.stage.as_str(), None, None, None, None),
        };
        let res = sqlx::query(
            r#"
            UPDATE capture_jobs
               SET stage = $3,
                   stage_progress = $4,
                   stage_started_at = CASE WHEN stage <> $3 THEN $10 ELSE stage_started_at END,
                   last_progress_at = $10,
                   fc_snapshot_version = COALESCE($5, fc_snapshot_version),
                   retryable = COALESCE($6, retryable),
                   error = COALESCE($7, error),
                   error_stage = COALESCE($8, error_stage),
                   result_bincode = COALESCE($9, result_bincode),
                   updated_at = $10
             WHERE id = $1 AND epoch = $2 AND stage NOT IN ('done', 'failed')
            "#,
        )
        .bind(report.job_id.as_uuid())
        .bind(report.epoch)
        .bind(stage)
        .bind(stage_progress)
        .bind(report.fc_snapshot_version.as_deref())
        .bind(retryable)
        .bind(error)
        .bind(error_stage)
        .bind(result_bincode)
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
    }

    async fn mirror_capture_progress_to_enable_job(
        &self,
        enable_job_id: Uuid,
        capture_phase: Option<&str>,
        warm_stage: Option<&str>,
        output_tail: Option<&str>,
        warm_stages: Option<&serde_json::Value>,
    ) -> Result<(), MetaError> {
        // UNFENCED on purpose (ADR 0084 P1b): `capture_jobs` owns
        // fencing/execution now, this is a cosmetic dashboard mirror the
        // heartbeat reconcile drives regardless of which coordinator pod
        // (if any) holds the enable job's claim. `COALESCE` so a report
        // with no rendered phase (`assigned`/`done`/`failed`) doesn't
        // blank the last-known warm-hook stage/output — or, ADR 0088
        // addendum, the last-known capture timeline.
        sqlx::query(
            r#"
            UPDATE enable_jobs
               SET capture_phase = COALESCE($2, capture_phase),
                   warm_stage = COALESCE($3, warm_stage),
                   output_tail = COALESCE($4, output_tail),
                   warm_stages = COALESCE($5, warm_stages),
                   updated_at = $6
             WHERE id = $1
            "#,
        )
        .bind(enable_job_id)
        .bind(capture_phase)
        .bind(warm_stage)
        .bind(output_tail)
        .bind(warm_stages)
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn place_capture_job(
        &self,
        id: CaptureJobId,
        candidates: &[HostId],
    ) -> Result<Option<CaptureJobRow>, MetaError> {
        // ADR 0084 (c): the reserving pick for a WAITING row. Lock the job
        // row FOR UPDATE (serializes concurrent placers of the same job),
        // then run the atomic 2D RAM/CPU best-fit over `candidates` using
        // the row's OWN stamped budgets, holding the host-row locks
        // `pick_host_2d` takes until commit. Only host_id-NULL rows are
        // placeable; a row already bound is returned unchanged (no
        // double-reserve).
        let cand: Vec<uuid::Uuid> = candidates.iter().map(|h| h.as_uuid()).collect();
        let now = self.clock.now_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let cur: Option<(Option<uuid::Uuid>, i64, i32)> = sqlx::query_as(
            r#"SELECT host_id, mem_budget_mib, cpu_budget_vcpus
                 FROM capture_jobs
                WHERE id = $1 AND stage NOT IN ('done', 'failed')
                FOR UPDATE"#,
        )
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        let Some((host_id, mem, cpu)) = cur else {
            tx.rollback().await.map_err(db_err)?;
            return Ok(None); // gone or already terminal
        };
        if host_id.is_some() {
            // Already placed (a racing placer won) — return unchanged.
            tx.rollback().await.map_err(db_err)?;
            return self.get_capture_job(id).await;
        }
        let picked = pick_host_2d(&mut tx, &cand, 0, mem, cpu as i64).await?;
        // Fit → bind host_id, clear the wait clock, re-anchor the
        // `assigned` deadline from dispatch. No fit → leave waiting,
        // stamping `waiting_since` on the first miss (COALESCE).
        let row = sqlx::query(
            r#"
            UPDATE capture_jobs
               SET host_id = $2,
                   waiting_since = CASE WHEN $2 IS NULL THEN COALESCE(waiting_since, $3) ELSE NULL END,
                   stage_started_at = CASE WHEN $2 IS NULL THEN stage_started_at ELSE $3 END,
                   last_progress_at = CASE WHEN $2 IS NULL THEN last_progress_at ELSE $3 END,
                   updated_at = $3
             WHERE id = $1 AND host_id IS NULL AND stage NOT IN ('done', 'failed')
            RETURNING id, enable_job_id, image_uri, manifest_digest, disk_manifest, image_config,
                      oci_defaults, host_id, mem_budget_mib, cpu_budget_vcpus, waiting_since, epoch, stage, stage_started_at, stage_progress,
                      last_progress_at, attempts, retryable, error, error_stage, fc_snapshot_version,
                      result_bincode, created_at, updated_at
            "#,
        )
        .bind(id.as_uuid())
        .bind(picked)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        row.map(|r| row::capture_job_from_row(&r)).transpose()
    }

    async fn reassign_capture_job(
        &self,
        id: CaptureJobId,
        expected_epoch: i64,
        candidates: &[HostId],
    ) -> Result<Option<CaptureJobRow>, MetaError> {
        // ADR 0084 (c): re-run the reserving 2D fit for the new host INSIDE
        // the same epoch-fenced write, so the host old→new swap is atomic
        // with the reservation. No fit ⇒ `host_id = NULL` (the row falls
        // into the waiting flow rather than failing). The `epoch + 1` bump
        // fences/tears down the abandoned attempt (host-side
        // `cancel_absent`); `attempts + 1` counts the real re-attempt.
        let cand: Vec<uuid::Uuid> = candidates.iter().map(|h| h.as_uuid()).collect();
        let now = self.clock.now_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let budgets: Option<(i64, i32)> = sqlx::query_as(
            r#"SELECT mem_budget_mib, cpu_budget_vcpus
                 FROM capture_jobs
                WHERE id = $1 AND epoch = $2 AND stage NOT IN ('done', 'failed')
                FOR UPDATE"#,
        )
        .bind(id.as_uuid())
        .bind(expected_epoch)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        let Some((mem, cpu)) = budgets else {
            tx.rollback().await.map_err(db_err)?;
            return Ok(None); // fence missed (already reassigned, or terminal)
        };
        let picked = pick_host_2d(&mut tx, &cand, 0, mem, cpu as i64).await?;
        let row = sqlx::query(
            r#"
            UPDATE capture_jobs
               SET host_id = $3,
                   waiting_since = CASE WHEN $3 IS NULL THEN COALESCE(waiting_since, $4) ELSE NULL END,
                   epoch = epoch + 1,
                   attempts = attempts + 1,
                   stage = 'assigned',
                   stage_started_at = $4,
                   last_progress_at = $4,
                   stage_progress = NULL,
                   updated_at = $4
             WHERE id = $1 AND epoch = $2 AND stage NOT IN ('done', 'failed')
            RETURNING id, enable_job_id, image_uri, manifest_digest, disk_manifest, image_config,
                      oci_defaults, host_id, mem_budget_mib, cpu_budget_vcpus, waiting_since, epoch, stage, stage_started_at, stage_progress,
                      last_progress_at, attempts, retryable, error, error_stage, fc_snapshot_version,
                      result_bincode, created_at, updated_at
            "#,
        )
        .bind(id.as_uuid())
        .bind(expected_epoch)
        .bind(picked)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        row.map(|r| row::capture_job_from_row(&r)).transpose()
    }

    async fn redrive_failed_capture_job(
        &self,
        id: CaptureJobId,
        expected_epoch: i64,
        candidates: &[HostId],
        max_attempts: u32,
    ) -> Result<Option<CaptureJobRow>, MetaError> {
        // Unlike `reassign_capture_job` (fenced `stage NOT IN
        // ('done','failed')`), this deliberately targets a TERMINAL
        // `failed` row and re-drives it — the automatic path's equivalent
        // of `retry_enable_job`'s DELETE-the-terminal-row escape. The
        // `attempts < $max` clause makes the budget atomic (an exhausted
        // job returns 0 rows → `None`); the `epoch + 1` bump keeps any
        // stale report from the abandoned attempt fenced out of
        // `record_capture_job_report`. Re-runs the reserving 2D fit for the
        // new host in the same fenced transaction (ADR 0084 (c)); no fit ⇒
        // `host_id = NULL` (waiting). Per-attempt fields are cleared and
        // timestamps reset to mirror a fresh `insert_capture_job` row.
        let cand: Vec<uuid::Uuid> = candidates.iter().map(|h| h.as_uuid()).collect();
        let now = self.clock.now_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let budgets: Option<(i64, i32)> = sqlx::query_as(
            r#"SELECT mem_budget_mib, cpu_budget_vcpus
                 FROM capture_jobs
                WHERE id = $1 AND epoch = $2 AND stage = 'failed' AND retryable AND attempts < $3
                FOR UPDATE"#,
        )
        .bind(id.as_uuid())
        .bind(expected_epoch)
        .bind(i64::from(max_attempts))
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        let Some((mem, cpu)) = budgets else {
            tx.rollback().await.map_err(db_err)?;
            return Ok(None); // exhausted, raced, or no longer retryable-failed
        };
        let picked = pick_host_2d(&mut tx, &cand, 0, mem, cpu as i64).await?;
        let row = sqlx::query(
            r#"
            UPDATE capture_jobs
               SET host_id = $3,
                   waiting_since = CASE WHEN $3 IS NULL THEN COALESCE(waiting_since, $5) ELSE NULL END,
                   epoch = epoch + 1,
                   attempts = attempts + 1,
                   stage = 'assigned',
                   stage_started_at = $5,
                   last_progress_at = $5,
                   stage_progress = NULL,
                   error = NULL,
                   error_stage = NULL,
                   retryable = NULL,
                   result_bincode = NULL,
                   fc_snapshot_version = NULL,
                   updated_at = $5
             WHERE id = $1 AND epoch = $2 AND stage = 'failed' AND retryable AND attempts < $4
            RETURNING id, enable_job_id, image_uri, manifest_digest, disk_manifest, image_config,
                      oci_defaults, host_id, mem_budget_mib, cpu_budget_vcpus, waiting_since, epoch, stage, stage_started_at, stage_progress,
                      last_progress_at, attempts, retryable, error, error_stage, fc_snapshot_version,
                      result_bincode, created_at, updated_at
            "#,
        )
        .bind(id.as_uuid())
        .bind(expected_epoch)
        .bind(picked)
        .bind(i64::from(max_attempts))
        .bind(now)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        row.map(|r| row::capture_job_from_row(&r)).transpose()
    }

    async fn expire_capture_job_stages(
        &self,
        budgets: &[(engram_core::types::CaptureJobStage, std::time::Duration)],
    ) -> Result<Vec<CaptureJobRow>, MetaError> {
        // Volumes are tiny (at most one active job per host, gated by
        // anti-affinity) — fetch the non-terminal candidate set and
        // filter in Rust rather than compose a per-stage CASE in SQL.
        // ADR 0084 (c): DISPATCHED rows only (`host_id IS NOT NULL`) — a
        // waiting row has no stage deadline, only the queue timeout
        // (`list_waiting_capture_jobs`).
        if budgets.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            r#"
            SELECT id, enable_job_id, image_uri, manifest_digest, disk_manifest, image_config,
                   oci_defaults, host_id, mem_budget_mib, cpu_budget_vcpus, waiting_since, epoch, stage, stage_started_at, stage_progress,
                   last_progress_at, attempts, retryable, error, error_stage, fc_snapshot_version,
                   result_bincode, created_at, updated_at
              FROM capture_jobs
             WHERE stage NOT IN ('done', 'failed') AND host_id IS NOT NULL
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        let now = self.clock.now_utc();
        let mut out = Vec::new();
        for r in &rows {
            let job = row::capture_job_from_row(r)?;
            let Some((_, budget)) = budgets.iter().find(|(stage, _)| *stage == job.stage) else {
                continue;
            };
            let age = now.signed_duration_since(job.last_progress_at);
            let over_budget = age.to_std().map(|age| age > *budget).unwrap_or(false);
            if over_budget {
                out.push(job);
            }
        }
        Ok(out)
    }

    async fn list_waiting_capture_jobs(&self) -> Result<Vec<CaptureJobRow>, MetaError> {
        // ADR 0084 (c): every WAITING (host_id NULL) non-terminal capture
        // job — the queue-timeout scan's read. Each is re-offered to
        // `place_capture_job` every tick; one whose `waiting_since` is
        // older than the queue timeout is failed with `CapacityTimeout`.
        let rows = sqlx::query(
            r#"
            SELECT id, enable_job_id, image_uri, manifest_digest, disk_manifest, image_config,
                   oci_defaults, host_id, mem_budget_mib, cpu_budget_vcpus, waiting_since, epoch, stage, stage_started_at, stage_progress,
                   last_progress_at, attempts, retryable, error, error_stage, fc_snapshot_version,
                   result_bincode, created_at, updated_at
              FROM capture_jobs
             WHERE stage NOT IN ('done', 'failed') AND host_id IS NULL
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row::capture_job_from_row).collect()
    }

    async fn capture_assignments_for_host(
        &self,
        host: HostId,
    ) -> Result<Vec<CaptureJobAssignment>, MetaError> {
        let rows: Vec<(Uuid, i64)> = sqlx::query_as(
            r#"
            SELECT id, epoch FROM capture_jobs
             WHERE host_id = $1 AND stage NOT IN ('done', 'failed')
            "#,
        )
        .bind(host.as_uuid())
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|(id, epoch)| CaptureJobAssignment {
                job_id: CaptureJobId(id),
                epoch,
            })
            .collect())
    }

    async fn hosts_with_live_capture_jobs(
        &self,
    ) -> Result<std::collections::HashSet<HostId>, MetaError> {
        // ADR 0084 (c): a WAITING job (`host_id NULL`) binds no host, so
        // it can't be in the anti-affinity set — exclude NULLs (and keep
        // the decode as a plain `Uuid`).
        let rows: Vec<(Uuid,)> = sqlx::query_as(
            r#"SELECT DISTINCT host_id FROM capture_jobs
                WHERE stage NOT IN ('done', 'failed') AND host_id IS NOT NULL"#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows.into_iter().map(|(id,)| HostId(id)).collect())
    }

    async fn set_enable_job_reuse_outcome(
        &self,
        enable_job_id: Uuid,
        outcome: &str,
    ) -> Result<(), MetaError> {
        // A plain write, not fenced by claimant — stamped once the job
        // has already reached a terminal enable-job state, so there's no
        // in-flight lease left to race against.
        sqlx::query("UPDATE enable_jobs SET reuse_outcome = $2, updated_at = $3 WHERE id = $1")
            .bind(enable_job_id)
            .bind(outcome)
            .bind(self.clock.now_utc())
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn upsert_cold_base(&self, row: ColdBaseRow) -> Result<(), MetaError> {
        sqlx::query(
            r#"
            INSERT INTO cold_bases (content_key, snapshot_id, disk_manifest, memory_manifest, fc_snapshot_version, captured_at, snapshot_bincode)
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT (content_key) DO UPDATE SET
                snapshot_id = EXCLUDED.snapshot_id,
                disk_manifest = EXCLUDED.disk_manifest,
                memory_manifest = EXCLUDED.memory_manifest,
                fc_snapshot_version = EXCLUDED.fc_snapshot_version,
                captured_at = EXCLUDED.captured_at,
                snapshot_bincode = EXCLUDED.snapshot_bincode
            "#,
        )
        .bind(&row.content_key)
        .bind(row.snapshot_id.as_uuid())
        .bind(&row.disk_manifest)
        .bind(&row.memory_manifest)
        .bind(&row.fc_snapshot_version)
        .bind(row.captured_at)
        .bind(&row.snapshot_bincode)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn get_cold_base(&self, content_key: &str) -> Result<Option<ColdBaseRow>, MetaError> {
        let row = sqlx::query(
            r#"
            SELECT content_key, snapshot_id, disk_manifest, memory_manifest, fc_snapshot_version, captured_at, snapshot_bincode
              FROM cold_bases
             WHERE content_key = $1
            "#,
        )
        .bind(content_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| row::cold_base_from_row(&r)).transpose()
    }

    async fn cold_base_snapshot_ids(&self) -> Result<Vec<SnapshotId>, MetaError> {
        let rows: Vec<(Uuid,)> = sqlx::query_as("SELECT snapshot_id FROM cold_bases")
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(rows.into_iter().map(|(id,)| SnapshotId(id)).collect())
    }

    async fn cold_base_manifest_refs(
        &self,
    ) -> Result<Vec<engram_core::types::manifest::ManifestRef>, MetaError> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT disk_manifest, memory_manifest FROM cold_bases")
                .fetch_all(&self.pool)
                .await
                .map_err(db_err)?;
        let mut refs = Vec::with_capacity(rows.len() * 2);
        for (disk, mem) in rows {
            for text in [disk, mem] {
                match text.parse::<engram_core::types::manifest::ManifestRef>() {
                    Ok(r) => refs.push(r),
                    Err(e) => {
                        tracing::warn!(
                            manifest = %text,
                            error = %e,
                            "cold_base_manifest_refs: unparseable manifest ref; skipping (GC \
                             pin-set collection continues with the rest)",
                        );
                    }
                }
            }
        }
        Ok(refs)
    }

    async fn cold_base_fc_version_changed(
        &self,
        disk_manifest: &str,
        current_fc_version: &str,
    ) -> Result<bool, MetaError> {
        let (exists,): (bool,) = sqlx::query_as(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM cold_bases
                 WHERE disk_manifest = $1 AND fc_snapshot_version <> $2
            )
            "#,
        )
        .bind(disk_manifest)
        .bind(current_fc_version)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(exists)
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
    // ADR 0079 (issue #543) — the durable per-session op log (the
    // successor of the retired session-lease ecosystem; migration 0093
    // dropped the table). `sessions.current_epoch` is the fencing epoch:
    // CAS-bumped in the SAME transaction that claims an op, appended
    // (`AND current_epoch = $e`) to every session-row write an op makes.
    // The `session_ops_one_running` partial unique index is the
    // correctness authority for "one running op per session"; the
    // pre-checks in these methods only keep the race cheap.
    // ----------------------------------------------------------------

    async fn op_enqueue_and_claim(
        &self,
        session_id: SessionId,
        kind: OpKind,
        payload: serde_json::Value,
        idempotency_key: Option<&str>,
        claimed_by: &str,
    ) -> Result<EnqueueOutcome, MetaError> {
        // Attempt insert + inline claim; a racing claimer from another
        // pod trips the one_running unique and aborts the WHOLE
        // transaction (insert included), so retry once enqueue-only —
        // the now-committed peer's completion re-drive (or the NOTIFY
        // this retry fires) picks the row up in order.
        match self
            .op_enqueue_tx(
                session_id,
                kind,
                &payload,
                idempotency_key,
                Some(claimed_by),
            )
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(e) if meta_is_unique_violation(&e) => {
                self.op_enqueue_tx(session_id, kind, &payload, idempotency_key, None)
                    .await
            }
            Err(e) => Err(e),
        }
    }

    async fn op_enqueue_and_claim_exclusive(
        &self,
        session_id: SessionId,
        kind: OpKind,
        payload: serde_json::Value,
        claimed_by: &str,
    ) -> Result<Option<SessionOp>, MetaError> {
        // ADR 0079 (review finding #8): insert + claim in ONE transaction,
        // rolling the WHOLE thing back (row included) if the lane is not
        // free. Inline claims are never idempotency-keyed. A racing peer
        // claim trips the `session_ops_one_running` unique on the stamp
        // and aborts here → mapped to `None` (busy), still no orphan row.
        let now = self.clock.now_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let insert = format!(
            "INSERT INTO session_ops (session_id, kind, payload)
             VALUES ($1, $2, $3)
             RETURNING {OP_COLUMNS}"
        );
        let row = sqlx::query(&insert)
            .bind(session_id.as_uuid())
            .bind(kind.as_str())
            .bind(&payload)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
        let op = row::session_op_from_row(&row)?;
        // The lane is free only if nothing is running AND no OTHER op is
        // queued for the session (ours was just inserted `queued`).
        let free: bool = sqlx::query_scalar(
            "SELECT NOT EXISTS (SELECT 1 FROM session_ops
                                 WHERE session_id = $1 AND state = 'running')
                AND NOT EXISTS (SELECT 1 FROM session_ops
                                 WHERE session_id = $1 AND state = 'queued' AND id <> $2)",
        )
        .bind(session_id.as_uuid())
        .bind(op.id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        if !free {
            // Busy: roll back so the just-inserted row disappears — the
            // executor can never grab a row we withdrew.
            tx.rollback().await.map_err(db_err)?;
            return Ok(None);
        }
        let epoch: i64 = sqlx::query_scalar(
            "UPDATE sessions SET current_epoch = current_epoch + 1
             WHERE id = $1 RETURNING current_epoch",
        )
        .bind(session_id.as_uuid())
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let stamp = format!(
            "UPDATE session_ops
                SET state = 'running', epoch = $2, claimed_by = $3,
                    claimed_at = $4, heartbeat_at = $4,
                    attempts = attempts + 1
              WHERE id = $1 AND state = 'queued'
             RETURNING {OP_COLUMNS}"
        );
        let stamped = match sqlx::query(&stamp)
            .bind(op.id)
            .bind(epoch)
            .bind(claimed_by)
            .bind(now)
            .fetch_one(&mut *tx)
            .await
        {
            Ok(row) => row::session_op_from_row(&row)?,
            Err(e) if is_unique_violation(&e) => return Ok(None),
            Err(e) => return Err(db_err(e)),
        };
        sqlx::query("SELECT pg_notify('session_ops', $1)")
            .bind(session_id.as_uuid().to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        match tx.commit().await {
            Ok(()) => Ok(Some(stamped)),
            Err(e) if is_unique_violation(&e) => Ok(None),
            Err(e) => Err(db_err(e)),
        }
    }

    async fn op_claim_head(
        &self,
        session_id: SessionId,
        claimed_by: &str,
    ) -> Result<Option<SessionOp>, MetaError> {
        let now = self.clock.now_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        // Head = smallest queued id, due. SKIP LOCKED keeps a race with
        // a peer's claim (or a concurrent enqueue_and_claim) cheap: if
        // the head is mid-claim elsewhere we either see nothing or fall
        // onto the one_running unique below and map it to None.
        let head = format!(
            "SELECT {OP_COLUMNS} FROM session_ops
              WHERE session_id = $1 AND state = 'queued'
                AND (not_before IS NULL OR not_before <= $2)
              ORDER BY id ASC LIMIT 1
              FOR UPDATE SKIP LOCKED"
        );
        let Some(row) = sqlx::query(&head)
            .bind(session_id.as_uuid())
            .bind(now)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?
        else {
            return Ok(None);
        };
        let op = row::session_op_from_row(&row)?;
        // Advisory pre-check (the partial unique enforces on the stamp):
        // don't burn an epoch bump when something is visibly running.
        let running: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM session_ops
                             WHERE session_id = $1 AND state = 'running')",
        )
        .bind(session_id.as_uuid())
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        if running {
            return Ok(None);
        }
        let epoch: i64 = sqlx::query_scalar(
            "UPDATE sessions SET current_epoch = current_epoch + 1
             WHERE id = $1 RETURNING current_epoch",
        )
        .bind(session_id.as_uuid())
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let stamp = format!(
            "UPDATE session_ops
                SET state = 'running', epoch = $2, claimed_by = $3,
                    claimed_at = $4, heartbeat_at = $4,
                    attempts = attempts + 1
              WHERE id = $1 AND state = 'queued'
             RETURNING {OP_COLUMNS}"
        );
        let stamped = match sqlx::query(&stamp)
            .bind(op.id)
            .bind(epoch)
            .bind(claimed_by)
            .bind(now)
            .fetch_one(&mut *tx)
            .await
        {
            Ok(row) => row::session_op_from_row(&row)?,
            // A peer won the one_running race between our pre-check and
            // the stamp — not claimable, not an error.
            Err(e) if is_unique_violation(&e) => return Ok(None),
            Err(e) => return Err(db_err(e)),
        };
        match tx.commit().await {
            Ok(()) => Ok(Some(stamped)),
            Err(e) if is_unique_violation(&e) => Ok(None),
            Err(e) => Err(db_err(e)),
        }
    }

    async fn op_due_sessions(&self) -> Result<Vec<SessionId>, MetaError> {
        let rows = sqlx::query(
            "SELECT DISTINCT session_id FROM session_ops
              WHERE state = 'queued'
                AND (not_before IS NULL OR not_before <= $1)
                AND NOT EXISTS (SELECT 1 FROM session_ops r
                                 WHERE r.session_id = session_ops.session_id
                                   AND r.state = 'running')",
        )
        .bind(self.clock.now_utc())
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.into_iter()
            .map(|r| {
                let id: uuid::Uuid = r.try_get(0).map_err(db_err)?;
                Ok(SessionId::from(id))
            })
            .collect()
    }

    async fn op_record_step(&self, op_id: i64, epoch: i64, step: &str) -> Result<bool, MetaError> {
        let res = sqlx::query(
            "UPDATE session_ops SET step = $3, heartbeat_at = $4
              WHERE id = $1 AND epoch = $2 AND state = 'running'",
        )
        .bind(op_id)
        .bind(epoch)
        .bind(step)
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
    }

    async fn op_heartbeat(&self, op_id: i64, epoch: i64) -> Result<bool, MetaError> {
        // Bump ONLY heartbeat_at — NOT step (the within-step liveness beat
        // must not clobber the crash-resume marker). Fenced.
        let res = sqlx::query(
            "UPDATE session_ops SET heartbeat_at = $3
              WHERE id = $1 AND epoch = $2 AND state = 'running'",
        )
        .bind(op_id)
        .bind(epoch)
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
    }

    async fn op_finish(
        &self,
        op_id: i64,
        epoch: i64,
        state: OpState,
        error: Option<&str>,
    ) -> Result<bool, MetaError> {
        let res = sqlx::query(
            "UPDATE session_ops SET state = $3, error = $4, finished_at = $5
              WHERE id = $1 AND epoch = $2 AND state = 'running'",
        )
        .bind(op_id)
        .bind(epoch)
        .bind(state.as_str())
        .bind(error)
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
    }

    async fn op_requeue_with_backoff(
        &self,
        op_id: i64,
        epoch: i64,
        backoff: std::time::Duration,
        error: &str,
    ) -> Result<bool, MetaError> {
        // Back to `queued` with the claim stamps cleared; `attempts`
        // stays (it was counted at claim) and `step` stays (the next
        // claimer resumes idempotent-from-step).
        let res = sqlx::query(
            "UPDATE session_ops
                SET state = 'queued',
                    not_before = $5 + make_interval(secs => $3),
                    error = $4,
                    epoch = NULL, claimed_by = NULL, heartbeat_at = NULL
              WHERE id = $1 AND epoch = $2 AND state = 'running'",
        )
        .bind(op_id)
        .bind(epoch)
        .bind(backoff.as_secs_f64())
        .bind(error)
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
    }

    async fn op_cancel_queued(
        &self,
        session_id: SessionId,
        kind: OpKind,
    ) -> Result<bool, MetaError> {
        // Every queued op of the kind — a cancelled verb has no business
        // running later from a duplicate row further down the queue.
        let res = sqlx::query(
            "UPDATE session_ops SET state = 'cancelled', finished_at = $3
              WHERE session_id = $1 AND kind = $2 AND state = 'queued'",
        )
        .bind(session_id.as_uuid())
        .bind(kind.as_str())
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
    }

    async fn op_wake_queued_kind(
        &self,
        session_id: SessionId,
        kind: OpKind,
    ) -> Result<u64, MetaError> {
        // ADR 0079 latency fix: pull a backed-off queued op's not_before
        // to now so the completion re-drive claims it immediately, then
        // NOTIFY so any replica's executor wakes even if the completion
        // re-drive already passed.
        let res = sqlx::query(
            "UPDATE session_ops SET not_before = $3
              WHERE session_id = $1 AND kind = $2 AND state = 'queued'
                AND not_before > $3",
        )
        .bind(session_id.as_uuid())
        .bind(kind.as_str())
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        let woken = res.rows_affected();
        if woken > 0 {
            sqlx::query("SELECT pg_notify('session_ops', $1)")
                .bind(session_id.as_uuid().to_string())
                .execute(&self.pool)
                .await
                .map_err(db_err)?;
        }
        Ok(woken)
    }

    async fn op_request_cancel_running(
        &self,
        session_id: SessionId,
        kind: OpKind,
    ) -> Result<bool, MetaError> {
        let res = sqlx::query(
            "UPDATE session_ops SET payload = jsonb_set(payload, '{_cancel}', 'true')
              WHERE session_id = $1 AND kind = $2 AND state = 'running'",
        )
        .bind(session_id.as_uuid())
        .bind(kind.as_str())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
    }

    async fn op_cancel_requested(&self, op_id: i64) -> Result<bool, MetaError> {
        let flag: Option<Option<bool>> = sqlx::query_scalar(
            "SELECT payload->>'_cancel' = 'true' FROM session_ops WHERE id = $1",
        )
        .bind(op_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        // Missing row or absent key (SQL NULL) both read as "no cancel".
        Ok(flag.flatten().unwrap_or(false))
    }

    async fn op_running_for(&self, session_id: SessionId) -> Result<Option<SessionOp>, MetaError> {
        let q = format!(
            "SELECT {OP_COLUMNS} FROM session_ops
              WHERE session_id = $1 AND state = 'running'"
        );
        let row = sqlx::query(&q)
            .bind(session_id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        row.map(|r| row::session_op_from_row(&r)).transpose()
    }

    /// ADR 0101 C: newest row (any state, any key) for `(session, kind)`
    /// — ids are monotonic, so `ORDER BY id DESC LIMIT 1` is the latest
    /// mint.
    async fn op_latest_for_kind(
        &self,
        session_id: SessionId,
        kind: engram_core::types::session_op::OpKind,
    ) -> Result<Option<SessionOp>, MetaError> {
        let q = format!(
            "SELECT {OP_COLUMNS} FROM session_ops
              WHERE session_id = $1 AND kind = $2
              ORDER BY id DESC LIMIT 1"
        );
        let row = sqlx::query(&q)
            .bind(session_id.as_uuid())
            .bind(kind.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        row.map(|r| row::session_op_from_row(&r)).transpose()
    }

    async fn op_get(&self, op_id: i64) -> Result<Option<SessionOp>, MetaError> {
        let q = format!("SELECT {OP_COLUMNS} FROM session_ops WHERE id = $1");
        let row = sqlx::query(&q)
            .bind(op_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        row.map(|r| row::session_op_from_row(&r)).transpose()
    }

    async fn op_cancel_by_id(&self, op_id: i64) -> Result<bool, MetaError> {
        let res = sqlx::query(
            "UPDATE session_ops SET state = 'cancelled', finished_at = $2
              WHERE id = $1 AND state = 'queued'",
        )
        .bind(op_id)
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
    }

    async fn op_pending_exists(
        &self,
        session_id: SessionId,
        kind: OpKind,
    ) -> Result<bool, MetaError> {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM session_ops
                             WHERE session_id = $1 AND kind = $2
                               AND state IN ('queued', 'running'))",
        )
        .bind(session_id.as_uuid())
        .bind(kind.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(exists)
    }

    async fn orphaned_pending_sessions(
        &self,
        older_than: std::time::Duration,
    ) -> Result<Vec<SessionId>, MetaError> {
        let rows = sqlx::query(
            "SELECT s.id FROM sessions s
              WHERE s.status = 'pending'
                AND s.last_active_at < $2 - make_interval(secs => $1)
                AND NOT EXISTS (
                    SELECT 1 FROM session_ops o
                     WHERE o.session_id = s.id
                       AND o.kind = 'create_boot'
                       AND o.state IN ('queued', 'running')
                )",
        )
        .bind(older_than.as_secs_f64())
        .bind(self.clock.now_utc())
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.into_iter()
            .map(|r| {
                let id: uuid::Uuid = r.try_get(0).map_err(db_err)?;
                Ok(SessionId::from(id))
            })
            .collect()
    }

    async fn op_reclaim_stale(
        &self,
        stale: std::time::Duration,
        claimed_by: &str,
    ) -> Result<Vec<SessionOp>, MetaError> {
        let now = self.clock.now_utc();
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        // SKIP LOCKED: a row a peer sweep is mid-reclaiming is theirs.
        // State stays 'running' through the re-stamp, so the one_running
        // index is never perturbed — the fence is the epoch bump.
        let select = format!(
            "SELECT {OP_COLUMNS} FROM session_ops
              WHERE state = 'running'
                AND heartbeat_at < $2 - make_interval(secs => $1)
              ORDER BY id ASC
              FOR UPDATE SKIP LOCKED"
        );
        let rows = sqlx::query(&select)
            .bind(stale.as_secs_f64())
            .bind(now)
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
        let mut reclaimed = Vec::with_capacity(rows.len());
        for row in &rows {
            let op = row::session_op_from_row(row)?;
            // Fence the stalled writer everywhere: bump the session's
            // epoch, then hand the row (and its recorded `step`) to us.
            let epoch: i64 = sqlx::query_scalar(
                "UPDATE sessions SET current_epoch = current_epoch + 1
                 WHERE id = $1 RETURNING current_epoch",
            )
            .bind(op.session_id.as_uuid())
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            let stamp = format!(
                "UPDATE session_ops
                    SET epoch = $2, claimed_by = $3, claimed_at = $4,
                        heartbeat_at = $4, attempts = attempts + 1
                  WHERE id = $1
                 RETURNING {OP_COLUMNS}"
            );
            let row = sqlx::query(&stamp)
                .bind(op.id)
                .bind(epoch)
                .bind(claimed_by)
                .bind(now)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)?;
            reclaimed.push(row::session_op_from_row(&row)?);
        }
        tx.commit().await.map_err(db_err)?;
        Ok(reclaimed)
    }

    async fn fenced_transition_session(
        &self,
        session_id: SessionId,
        epoch: i64,
        to: SessionState,
        disposition: BindingDisposition,
    ) -> Result<Option<SessionState>, MetaError> {
        // Same legality semantics as `transition_session` (SELECT-then-
        // UPDATE under the row lock, `try_transition_to` gating the
        // write), plus the epoch fence on the UPDATE. An illegal
        // transition is a `Conflict` (a real bug in the caller); a fenced
        // write (0 rows because `current_epoch` moved) is `Ok(None)` —
        // the op executor was superseded and must stop silently.
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let row = sqlx::query(
            "SELECT status, current_epoch, sandbox_id FROM sessions WHERE id = $1 FOR UPDATE",
        )
        .bind(session_id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .ok_or(MetaError::NotFound)?;
        // ADR 0079 (review finding #3): the FENCE CHECK MUST PRECEDE the
        // legality check. A fenced executor whose successor already
        // transitioned would otherwise see the successor's *resulting*
        // state and fail `try_transition_to` with a bare `Conflict` (no
        // `fenced:` prefix) — callers matching `starts_with("fenced:")`
        // would drop to the generic Err arm and fire the compensation
        // (`abort_inflight_snapshot`) that a fenced executor MUST NOT run
        // (the 89f7984d brick class). Reading `current_epoch` in this same
        // FOR UPDATE row and returning `Ok(None)` on a mismatch makes
        // epoch-staleness the silent-stop path regardless of the
        // successor's resulting state.
        let stored_epoch: i64 = row.try_get("current_epoch").map_err(|e| {
            MetaError::Serialization(format!("fenced_transition_session: read epoch: {e}"))
        })?;
        if stored_epoch != epoch {
            return Ok(None);
        }
        let current_raw: String = row.try_get("status").map_err(|e| {
            MetaError::Serialization(format!("fenced_transition_session: read current: {e}"))
        })?;
        let current = row::parse_session_state_for_lib(&current_raw)?;
        current.try_transition_to(to).map_err(|e| {
            tracing::warn!(
                session_id = %session_id,
                from = %current.as_str(),
                to = %to.as_str(),
                "rejected illegal session state transition (fenced path)"
            );
            MetaError::Conflict(e.to_string())
        })?;
        // #896 / ADR 0090 addendum: disposition legality under the same
        // row lock (after the fence check — a fenced executor stops
        // silently before any legality noise).
        let arriving_bound: Option<uuid::Uuid> = row.try_get("sandbox_id").map_err(|e| {
            MetaError::Serialization(format!("fenced_transition_session: read sandbox_id: {e}"))
        })?;
        if !to.binding_disposition_legal(arriving_bound.is_some(), disposition) {
            return Err(MetaError::Conflict(format!(
                "illegal binding disposition {disposition:?} into {} on a bound row",
                to.as_str()
            )));
        }
        // The same UPDATE `transition_session` commits (counter resets
        // included), fenced by the epoch predicate.
        let n = sqlx::query(
            r#"
            UPDATE sessions
               SET status = $2,
                   last_active_at = $4,
                   updated_at = $4,
                   sandbox_id = CASE WHEN $5 THEN NULL ELSE sandbox_id END,
                   evac_attempts = CASE WHEN $2 = 'evacuating' THEN 0 ELSE evac_attempts END,
                   evict_attempts = CASE WHEN $2 = 'evicting' THEN 0 ELSE evict_attempts END,
                   queued_at = CASE WHEN $2 = 'queued' THEN $4 ELSE queued_at END,
                   queue_origin = CASE WHEN $2 = 'queued'
                                  THEN COALESCE(queue_origin, 'create')
                                  ELSE queue_origin END
             WHERE id = $1 AND current_epoch = $3
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(to.as_str())
        .bind(epoch)
        .bind(self.clock.now_utc())
        .bind(matches!(disposition, BindingDisposition::Detach))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        tx.commit().await.map_err(db_err)?;
        if n == 0 {
            return Ok(None);
        }
        // Mirror `transition_session`'s post-commit queue-scanner wake:
        // a session leaving a memory-reserving state frees its budget.
        if current.reserves_host_memory() && !to.reserves_host_memory() {
            self.notify_placement_changed("session_freed").await;
        }
        Ok(Some(current))
    }

    async fn fenced_transition_session_with_events(
        &self,
        session_id: SessionId,
        epoch: i64,
        to: SessionState,
        disposition: BindingDisposition,
        events: &[(String, serde_json::Value)],
    ) -> Result<Option<(SessionState, Vec<i64>)>, MetaError> {
        // `fenced_transition_session` with the event appends folded into
        // the SAME transaction (see the trait doc for the event-loss race
        // this closes). The FOR UPDATE row lock spans the fence check, the
        // status flip, and every idx allocation, so the appended events
        // are contiguous and no successor can interleave; the in-SQL
        // pg_notify calls fire only on COMMIT, so subscribers never hear
        // about a rolled-back transition.
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let row = sqlx::query(
            "SELECT status, current_epoch, sandbox_id FROM sessions WHERE id = $1 FOR UPDATE",
        )
        .bind(session_id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .ok_or(MetaError::NotFound)?;
        // Fence check BEFORE legality — same rationale as
        // `fenced_transition_session` (ADR 0079 review finding #3).
        let stored_epoch: i64 = row.try_get("current_epoch").map_err(|e| {
            MetaError::Serialization(format!(
                "fenced_transition_session_with_events: read epoch: {e}"
            ))
        })?;
        if stored_epoch != epoch {
            return Ok(None);
        }
        let current_raw: String = row.try_get("status").map_err(|e| {
            MetaError::Serialization(format!(
                "fenced_transition_session_with_events: read current: {e}"
            ))
        })?;
        let current = row::parse_session_state_for_lib(&current_raw)?;
        current.try_transition_to(to).map_err(|e| {
            tracing::warn!(
                session_id = %session_id,
                from = %current.as_str(),
                to = %to.as_str(),
                "rejected illegal session state transition (fenced-with-events path)"
            );
            MetaError::Conflict(e.to_string())
        })?;
        // #896 / ADR 0090 addendum: disposition legality under the same
        // row lock, after the fence check.
        let arriving_bound: Option<uuid::Uuid> = row.try_get("sandbox_id").map_err(|e| {
            MetaError::Serialization(format!(
                "fenced_transition_session_with_events: read sandbox_id: {e}"
            ))
        })?;
        if !to.binding_disposition_legal(arriving_bound.is_some(), disposition) {
            return Err(MetaError::Conflict(format!(
                "illegal binding disposition {disposition:?} into {} on a bound row",
                to.as_str()
            )));
        }
        let now = self.clock.now_utc();
        let n = sqlx::query(
            r#"
            UPDATE sessions
               SET status = $2,
                   last_active_at = $4,
                   updated_at = $4,
                   sandbox_id = CASE WHEN $5 THEN NULL ELSE sandbox_id END,
                   evac_attempts = CASE WHEN $2 = 'evacuating' THEN 0 ELSE evac_attempts END,
                   evict_attempts = CASE WHEN $2 = 'evicting' THEN 0 ELSE evict_attempts END,
                   queued_at = CASE WHEN $2 = 'queued' THEN $4 ELSE queued_at END,
                   queue_origin = CASE WHEN $2 = 'queued'
                                  THEN COALESCE(queue_origin, 'create')
                                  ELSE queue_origin END
             WHERE id = $1 AND current_epoch = $3
            "#,
        )
        .bind(session_id.as_uuid())
        .bind(to.as_str())
        .bind(epoch)
        .bind(now)
        .bind(matches!(disposition, BindingDisposition::Detach))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        if n == 0 {
            return Ok(None);
        }
        let mut indices = Vec::with_capacity(events.len());
        for (kind, payload) in events {
            let row = sqlx::query(
                r#"
                WITH next AS (
                    UPDATE sessions
                       SET next_event_idx = next_event_idx + 1,
                           updated_at = $4,
                           last_event_at = $4
                     WHERE id = $1
                 RETURNING next_event_idx - 1 AS allocated_idx, recovery_epoch
                ),
                inserted AS (
                    INSERT INTO session_events (session_id, idx, kind, payload, recovery_epoch, created_at)
                    SELECT $1, allocated_idx, $2, $3, recovery_epoch, $4 FROM next
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
            .bind(now)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            indices.push(sqlx::Row::try_get(&row, "idx").map_err(db_err)?);
        }
        tx.commit().await.map_err(db_err)?;
        if current.reserves_host_memory() && !to.reserves_host_memory() {
            self.notify_placement_changed("session_freed").await;
        }
        Ok(Some((current, indices)))
    }

    async fn fenced_assign_sandbox(
        &self,
        session_id: SessionId,
        epoch: i64,
        sandbox_id: Option<SandboxId>,
        host_id: Option<HostId>,
    ) -> Result<bool, MetaError> {
        let res = sqlx::query(
            "UPDATE sessions
                SET sandbox_id = $2, host_id = $3, last_active_at = $5, updated_at = $5
              WHERE id = $1 AND current_epoch = $4",
        )
        .bind(session_id.as_uuid())
        .bind(sandbox_id.map(|s| s.as_uuid()))
        .bind(host_id.map(|h| h.as_uuid()))
        .bind(epoch)
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
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
                    live_disk_manifest_at      = $5
              WHERE id         = $1
                AND sandbox_id = $2",
        )
        .bind(session_id.as_uuid())
        .bind(sandbox_id.as_uuid())
        .bind(manifest_ref.manifest_id)
        .bind(manifest_ref.version as i64)
        .bind(self.clock.now_utc())
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
            "INSERT INTO chunk_gc_candidates (content_hash, first_seen_at, last_seen_at)
             VALUES ($1, $2, $2)
             ON CONFLICT (content_hash) DO UPDATE SET last_seen_at = $2",
        )
        .bind(hash.as_slice())
        .bind(self.clock.now_utc())
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

    /// ADR 0035 §5 + ADR 0055 P2 + ADR 0062 + ADR 0035 amendment D2: distinct bundle
    /// generations referenced by any snapshot row **∪ every live
    /// `mount_catalog` skill** ∪ **the current harness catalog generation**
    /// (`dyn_0`) **∪ every live host's per-sandbox attachments ∪ every live
    /// host's bake stamp** — so a registered-but-currently-unused uploaded
    /// skill, the harness catalog a fresh session will mount, a
    /// running-but-unsnapshotted sandbox's attached generations, and a
    /// staged-fleet-wide stamp all survive the bundle GC and the host
    /// sweep. jsonb unnest in SQL so the coord never pages any table; the
    /// result is a handful of refs. Catalog pins carry the skill `name` as
    /// a cosmetic `drive_id` (the host stages by `sha256`); a sha pinned by
    /// several legs appears once per distinct drive_id, which the GC (keyed
    /// on sha) and the host supervisor (stages by sha, idempotent) both
    /// collapse.
    async fn bundle_pin_set(
        &self,
    ) -> Result<Vec<engram_core::types::sandbox::AuxBundleRef>, MetaError> {
        // ADR 0035 amendment D2: two hosts-table legs on top of the ADR 0035/0055/0062
        // unions. `sandbox_bundles` pins what each RUNNING sandbox has
        // attached (a live-but-unsnapshotted sandbox previously pinned
        // nothing — the 2026-08-10 chain_poisoned gap); `current_bundles`
        // pins every live host's bake stamp so a generation staged
        // fleet-wide survives until it leaves every stamp. Both legs
        // exclude `dead` hosts: a dead host's sandboxes are unbound and
        // its stamp is unreachable, so only snapshots keep those pins.
        let rows = sqlx::query_as::<_, (String, String)>(
            "SELECT DISTINCT b->>'drive_id', b->>'sha256'
               FROM snapshots, jsonb_array_elements(aux_bundles) AS b
             UNION
             SELECT name, sha256 FROM mount_catalog WHERE deleted_at IS NULL
             UNION
             SELECT name, squashfs_sha256 FROM harness_catalog WHERE deleted_at IS NULL
             UNION
             SELECT b->>'drive_id', b->>'sha256'
               FROM hosts h,
                    jsonb_array_elements(h.sandbox_bundles) AS sb,
                    jsonb_array_elements(sb->'bundles') AS b
              WHERE h.status IN ('ready','draining')
             UNION
             SELECT b->>'drive_id', b->>'sha256'
               FROM hosts h, jsonb_array_elements(h.current_bundles) AS b
              WHERE h.status IN ('ready','draining')",
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
            "UPDATE mount_catalog SET deleted_at = $2
               WHERE name = $1 AND deleted_at IS NULL",
        )
        .bind(name)
        .bind(self.clock.now_utc())
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
            "UPDATE harness_catalog SET deleted_at = $2
               WHERE name = $1 AND deleted_at IS NULL",
        )
        .bind(name)
        .bind(self.clock.now_utc())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected() > 0)
    }

    /// ADR 0035 §5: sticky-first-seen candidate upsert (bundle
    /// flavor of `upsert_chunk_gc_candidate`).
    async fn upsert_bundle_gc_candidate(&self, sha256: &str) -> Result<(), MetaError> {
        sqlx::query(
            "INSERT INTO bundle_gc_candidates (sha256, first_seen_at, last_seen_at)
             VALUES ($1, $2, $2)
             ON CONFLICT (sha256) DO UPDATE SET last_seen_at = $2",
        )
        .bind(sha256)
        .bind(self.clock.now_utc())
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
            "INSERT INTO snapshot_blob_gc_candidates (snapshot_id, first_seen_at, last_seen_at)
             VALUES ($1, $2, $2)
             ON CONFLICT (snapshot_id) DO UPDATE SET last_seen_at = $2",
        )
        .bind(id.as_uuid())
        .bind(self.clock.now_utc())
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

/// ADR 0067: map a `session_outbox` row. Free fn (not a `FromRow`) to
/// keep the runtime-query style the rest of this store uses.
fn outbox_row_from_pg(
    r: &sqlx::postgres::PgRow,
) -> Result<engram_core::types::outbox::OutboxRow, MetaError> {
    let kind_s: String = r.try_get("kind").map_err(db_err)?;
    let kind = engram_core::types::outbox::OutboxKind::parse(&kind_s).ok_or_else(|| {
        MetaError::Serialization(format!("unknown session_outbox.kind {kind_s:?}"))
    })?;
    let session_id: uuid::Uuid = r.try_get("session_id").map_err(db_err)?;
    Ok(engram_core::types::outbox::OutboxRow {
        prompt_id: r.try_get("prompt_id").map_err(db_err)?,
        session_id: SessionId::from(session_id),
        kind,
        payload: r.try_get("payload").map_err(db_err)?,
        created_at: r.try_get("created_at").map_err(db_err)?,
        attempts: r.try_get("attempts").map_err(db_err)?,
        not_before: r.try_get("not_before").map_err(db_err)?,
        delivered_at: r.try_get("delivered_at").map_err(db_err)?,
        acked_at: r.try_get("acked_at").map_err(db_err)?,
    })
}

fn oauth_credential_from_pg(
    row: &sqlx::postgres::PgRow,
) -> Result<engram_core::types::oauth::SealedOAuthCredential, MetaError> {
    use std::str::FromStr;
    let subject_kind: String = row.try_get("subject_kind").map_err(db_err)?;
    let metadata: serde_json::Value = row.try_get("account_metadata").map_err(db_err)?;
    Ok(engram_core::types::oauth::SealedOAuthCredential {
        key: engram_core::types::oauth::OAuthCredentialKey {
            subject_kind: engram_core::types::oauth::OAuthSubjectKind::from_str(&subject_kind)
                .map_err(MetaError::Serialization)?,
            subject_id: row.try_get("subject_id").map_err(db_err)?,
            provider: row.try_get("provider").map_err(db_err)?,
        },
        wrapped_dek: row.try_get("wrapped_dek").map_err(db_err)?,
        nonce: row.try_get("nonce").map_err(db_err)?,
        ciphertext: row.try_get("ciphertext").map_err(db_err)?,
        key_id: row.try_get("key_id").map_err(db_err)?,
        metadata: serde_json::from_value(metadata)
            .map_err(|e| MetaError::Serialization(e.to_string()))?,
        version: row.try_get("version").map_err(db_err)?,
        created_at: row.try_get("created_at").map_err(db_err)?,
        updated_at: row.try_get("updated_at").map_err(db_err)?,
        revoked_at: row.try_get("revoked_at").map_err(db_err)?,
        expires_at: row.try_get("expires_at").map_err(db_err)?,
        broken_at: row.try_get("broken_at").map_err(db_err)?,
        broken_reason: row.try_get("broken_reason").map_err(db_err)?,
    })
}

fn oauth_flow_from_pg(
    row: &sqlx::postgres::PgRow,
) -> Result<engram_core::types::oauth::OAuthFlow, MetaError> {
    use std::str::FromStr;
    let subject_kind: String = row.try_get("subject_kind").map_err(db_err)?;
    let status: String = row.try_get("status").map_err(db_err)?;
    Ok(engram_core::types::oauth::OAuthFlow {
        id: row.try_get("id").map_err(db_err)?,
        key: engram_core::types::oauth::OAuthCredentialKey {
            subject_kind: engram_core::types::oauth::OAuthSubjectKind::from_str(&subject_kind)
                .map_err(MetaError::Serialization)?,
            subject_id: row.try_get("subject_id").map_err(db_err)?,
            provider: row.try_get("provider").map_err(db_err)?,
        },
        owner_replica: row.try_get("owner_replica").map_err(db_err)?,
        lease_expires_at: row.try_get("lease_expires_at").map_err(db_err)?,
        expires_at: row.try_get("expires_at").map_err(db_err)?,
        status: engram_core::types::oauth::OAuthFlowStatus::from_str(&status)
            .map_err(MetaError::Serialization)?,
        error_code: row.try_get("error_code").map_err(db_err)?,
        created_at: row.try_get("created_at").map_err(db_err)?,
        updated_at: row.try_get("updated_at").map_err(db_err)?,
    })
}
