//! The `MetadataStore` impl. One lock per method; SQL semantics mirrored
//! from `PostgresStore` (see each method's comment for the statement it
//! replicates). Grouped by table family; unimplemented PG-semantic
//! methods panic via `sim_unimplemented!` at the bottom.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use engram_core::traits::metadata::{
    CreateDisposition, DisableEnabledImageOutcome, GcCandidateRow, MetadataStore, PlacementNoFit,
    SessionCreateWriteSet, SnapshotTotals,
};
use engram_core::types::capability::Capability;
use engram_core::types::capture_job::{
    CaptureJobAssignment, CaptureJobReport, CaptureJobRow, CaptureJobStage, ColdBaseRow,
    NewCaptureJob,
};
use engram_core::types::event::{ArtifactRow, PersistedEvent};
use engram_core::types::host::{HostRecord, HostStatus, ReservedBudget};
use engram_core::types::ids::CaptureJobId;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::registry::{EnableJob, EnableJobState};
use engram_core::types::registry::{EnabledImage, RegistryCredential, SessionSecrets};
use engram_core::types::session::SandboxAssignment;
use engram_core::types::session::{
    DeleteHostOutcome, QueueOrigin, QueuedSession, Session, SessionSpec, SessionState,
};
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::SandboxId;
use engram_core::{HostId, MetaError, SessionId, SnapshotId};

use super::{SessRow, SimDb, SimMetadataStore};

/// Issue #722 reservation predicate, shared by pick/reserved/no-fit:
/// a `pending` counts while fresh OR while a live create_boot op
/// exists — a written-off pending can then never boot (the reclaim
/// sweep fails stale op-less orphans).
fn pending_counts(db: &SimDb, row: &SessRow, now: DateTime<Utc>) -> bool {
    if row.session.status != SessionState::Pending {
        return true;
    }
    if row.session.last_active_at > now - chrono::Duration::minutes(10) {
        return true;
    }
    use engram_core::types::session_op::{OpKind, OpState};
    db.session_ops.values().any(|o| {
        o.session_id == row.session.id
            && o.kind == OpKind::CreateBoot
            && matches!(o.state, OpState::Queued | OpState::Running)
    })
}

/// States that hold a host-memory reservation — mirrors
/// `PostgresStore::host_memory_reserving_states()` /
/// `SessionState::reserves_host_memory`.
fn reserves(state: SessionState) -> bool {
    state.reserves_host_memory()
}

impl SimMetadataStore {
    fn pending_counts_row(db: &SimDb, row: &SessRow, now: DateTime<Utc>) -> bool {
        pending_counts(db, row, now)
    }

    /// Mirror of `pick_host_2d` + `choose_placement_host`: candidates
    /// filtered to ready|draining and not cordoned; reservations summed
    /// over reserving-state sessions (a `pending` older than 10 minutes
    /// is crash-orphaned and excluded, keyed on `last_active_at`);
    /// best-fit = smallest allocatable-minus-reserved RAM that fits both
    /// budgets, affinity tier (`candidates[..affinity_len]`) first, then
    /// the rest, then any unmeasured host (allocatable == 0) as a last
    /// resort.
    ///
    /// DIVERGENCE (documented): PostgresStore also sums non-terminal
    /// `capture_jobs` reservations; the sim has no capture-job table
    /// yet, so conformance scenarios must not create capture jobs.
    fn pick_host_2d(
        db: &SimDb,
        candidates: &[HostId],
        affinity_len: usize,
        mem_budget_mib: i64,
        cpu_budget_vcpus: i64,
        now: DateTime<Utc>,
    ) -> Option<HostId> {
        let eligible: Vec<&HostRecord> = candidates
            .iter()
            .filter_map(|id| db.hosts.get(id))
            .filter(|h| matches!(h.status, HostStatus::Ready | HostStatus::Draining) && !h.cordoned)
            .collect();
        if eligible.is_empty() {
            return None;
        }
        let mut reserved: std::collections::BTreeMap<HostId, (i64, i64)> =
            std::collections::BTreeMap::new();
        for row in db.sessions.values() {
            let Some(host) = row.session.host_id else {
                continue;
            };
            let st = row.session.status;
            let counts = reserves(st) && Self::pending_counts_row(db, row, now);
            if counts {
                let e = reserved.entry(host).or_default();
                e.0 += row.mem_budget_mib;
                e.1 += i64::from(row.cpu_budget_vcpus);
            }
        }
        let fits = |h: &&HostRecord| -> Option<i64> {
            let alloc = h.utilization.allocatable_mib as i64;
            if alloc <= 0 {
                return None; // unmeasured — last-resort tier
            }
            let (mem_res, cpu_res) = reserved.get(&h.id).copied().unwrap_or((0, 0));
            let free_mem = alloc - mem_res;
            // CPU budgets are OVERCOMMITTED (host_cpu_budget = vcpus x
            // factor) — conformance caught SimMeta using raw vcpus.
            let cpu_budget = engram_core::types::host::host_cpu_budget(h.total_vcpus);
            let free_cpu = cpu_budget - cpu_res;
            (free_mem >= mem_budget_mib && free_cpu >= cpu_budget_vcpus).then_some(free_mem)
        };
        let best_of = |tier: &[&HostRecord]| -> Option<HostId> {
            tier.iter()
                .filter_map(|h| fits(h).map(|free| (free, h.id)))
                .min_by_key(|(free, id)| (*free, *id))
                .map(|(_, id)| id)
        };
        let (affinity, rest) = eligible.split_at(affinity_len.min(eligible.len()));
        best_of(affinity).or_else(|| best_of(rest)).or_else(|| {
            // Unmeasured hosts as last resort, affinity order.
            eligible
                .iter()
                .find(|h| h.utilization.allocatable_mib == 0)
                .map(|h| h.id)
        })
    }
}

#[async_trait]
impl MetadataStore for SimMetadataStore {
    // ================= sessions =================

    /// `INSERT INTO sessions (id, status='pending', image_uri, mode,
    /// created_at, last_active_at) VALUES (...)`.
    async fn create_session(&self, spec: SessionSpec) -> Result<SessionId, MetaError> {
        self.gate()?;
        let now = self.now();
        let id = SessionId::from(self.entropy.uuid());
        let mut db = self.db.lock();
        db.sessions.insert(
            id,
            SessRow {
                session: Session {
                    id,
                    status: SessionState::Pending,
                    host_id: None,
                    sandbox_id: None,
                    image: spec.image,
                    mode: spec.mode,
                    created_at: now,
                    last_active_at: now,
                    live_disk_manifest: None,
                    park_rung: 0,
                    parked_at: None,
                    suggested_title: None,
                },
                mem_budget_mib: 0,
                cpu_budget_vcpus: 0,
                queue_origin: None,
                queued_at: None,
                missing_strikes: 0,
                current_epoch: 0,
                binding_epoch: 0,
                next_event_idx: 0,
                recovery_epoch: 0,
                shell_pinned_until: None,
                durable_head: None,
                evac_attempts: 0,
                evict_attempts: 0,
                last_event_at: None,
                updated_at: now,
            },
        );
        Ok(id)
    }

    /// `UPDATE ... SET status='created', sandbox_id=$2, last_active_at=$3
    /// WHERE id=$1 AND status='pending'`; 0 rows -> NotFound.
    async fn transition_session_created(
        &self,
        id: SessionId,
        sandbox_id: engram_core::SandboxId,
    ) -> Result<(), MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let row = db.sessions.get_mut(&id).ok_or(MetaError::NotFound)?;
        if row.session.status != SessionState::Pending {
            return Err(MetaError::NotFound);
        }
        row.session.status = SessionState::Created;
        row.session.sandbox_id = Some(sandbox_id);
        row.session.last_active_at = now;
        db.transition_log.push(super::TransitionLogEntry {
            session: id,
            from: SessionState::Pending,
            to: SessionState::Created,
            exempt: false,
        });
        Ok(())
    }

    async fn get_session(&self, id: SessionId) -> Result<Session, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        db.sessions
            .get(&id)
            .map(|r| r.session.clone())
            .ok_or(MetaError::NotFound)
    }

    /// `WHERE status IN (pending,created,active,unreachable,idle,
    /// evacuating,evicting)` — queued and host_lost excluded.
    async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError> {
        self.gate()?;
        use SessionState::*;
        let db = self.db.lock();
        Ok(db
            .sessions
            .values()
            .filter(|r| {
                matches!(
                    r.session.status,
                    Pending | Created | Active | Unreachable | Idle | Evacuating | Evicting
                )
            })
            .map(|r| r.session.clone())
            .collect())
    }

    /// One transaction: pick_host_2d, then INSERT the session row as
    /// pending-on-host (Placed) or queued (Queued) + satellites.
    async fn reserve_and_persist_create(
        &self,
        ws: SessionCreateWriteSet,
        candidates: &[HostId],
        affinity_len: usize,
    ) -> Result<CreateDisposition, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let picked = Self::pick_host_2d(
            &db,
            candidates,
            affinity_len,
            ws.mem_budget_mib,
            i64::from(ws.cpu_budget_vcpus),
            now,
        );
        let (status, host_id, queue_origin, queued_at) = match picked {
            Some(h) => (SessionState::Pending, Some(h), None, None),
            None => (
                SessionState::Queued,
                None,
                Some(QueueOrigin::Create),
                Some(now),
            ),
        };
        db.sessions.insert(
            ws.session_id,
            SessRow {
                session: Session {
                    id: ws.session_id,
                    status,
                    host_id,
                    sandbox_id: None,
                    image: ws.spec.image.clone(),
                    mode: ws.spec.mode,
                    created_at: now,
                    last_active_at: now,
                    live_disk_manifest: None,
                    park_rung: 0,
                    parked_at: None,
                    suggested_title: None,
                },
                mem_budget_mib: ws.mem_budget_mib,
                cpu_budget_vcpus: ws.cpu_budget_vcpus,
                queue_origin,
                queued_at,
                missing_strikes: 0,
                current_epoch: 0,
                binding_epoch: 0,
                next_event_idx: 0,
                recovery_epoch: 0,
                shell_pinned_until: None,
                durable_head: None,
                evac_attempts: 0,
                evict_attempts: 0,
                last_event_at: None,
                updated_at: now,
            },
        );
        db.runtime_specs.insert(ws.session_id, ws.runtime_spec);
        if let Some(secrets) = ws.sealed_secrets {
            db.session_secrets.insert(ws.session_id, secrets);
        }
        if !ws.capabilities.is_empty() {
            db.session_capabilities
                .insert(ws.session_id, ws.capabilities.clone());
        }
        if let Some(policy) = &ws.integration_policy_json {
            db.session_integration_policy
                .insert(ws.session_id, policy.clone());
        }
        drop(db);
        match picked {
            Some(h) => Ok(CreateDisposition::Placed(h)),
            None => {
                // Post-commit `notify_placement_changed("enqueued")`.
                self.notify("placement_changed", "enqueued");
                Ok(CreateDisposition::Queued)
            }
        }
    }

    /// `DELETE ... WHERE id=$1 AND status='pending' AND sandbox_id IS NULL`.
    async fn delete_pending_session(&self, session_id: SessionId) -> Result<(), MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        let remove = db.sessions.get(&session_id).is_some_and(|r| {
            r.session.status == SessionState::Pending && r.session.sandbox_id.is_none()
        });
        if remove {
            db.sessions.remove(&session_id);
        }
        Ok(())
    }

    /// SELECT FOR UPDATE + legality + UPDATE. Illegal -> Conflict;
    /// missing -> NotFound; returns the PREVIOUS state.
    async fn transition_session(
        &self,
        id: SessionId,
        target: SessionState,
    ) -> Result<SessionState, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let row = db.sessions.get_mut(&id).ok_or(MetaError::NotFound)?;
        let current = row.session.status;
        current
            .try_transition_to(target)
            .map_err(|e| MetaError::Conflict(e.to_string()))?;
        row.session.status = target;
        let log_entry = super::TransitionLogEntry {
            session: id,
            from: current,
            to: target,
            exempt: false,
        };
        row.session.last_active_at = now;
        row.updated_at = now;
        if target == SessionState::Evacuating {
            row.evac_attempts = 0;
        }
        if target == SessionState::Evicting {
            row.evict_attempts = 0;
        }
        if target == SessionState::Queued {
            // Mirror of PostgresStore's entering-Queued stamp (ADR 0098
            // D4 conformance finding): FIFO columns are always valid.
            row.queued_at = Some(now);
            row.queue_origin = Some(row.queue_origin.unwrap_or(QueueOrigin::Create));
        }
        db.transition_log.push(log_entry);
        drop(db);
        if reserves(current) && !reserves(target) {
            self.notify("placement_changed", "session_freed");
        }
        Ok(current)
    }

    /// `UPDATE sessions SET host_id=$2, updated_at=$3 WHERE id=$1`.
    async fn assign_session_host(
        &self,
        id: SessionId,
        host_id: Option<HostId>,
    ) -> Result<(), MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let row = db.sessions.get_mut(&id).ok_or(MetaError::NotFound)?;
        row.session.host_id = host_id;
        row.updated_at = now;
        Ok(())
    }

    /// Bind: set sandbox + reset strikes. Unbind (None): also clears the
    /// live disk manifest and bumps chunk_generation (same tx).
    async fn assign_session_sandbox(
        &self,
        id: SessionId,
        sandbox_id: Option<engram_core::SandboxId>,
    ) -> Result<(), MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let row = db.sessions.get_mut(&id).ok_or(MetaError::NotFound)?;
        row.session.sandbox_id = sandbox_id;
        row.missing_strikes = 0;
        row.updated_at = now;
        if sandbox_id.is_none() {
            row.session.live_disk_manifest = None;
            db.chunk_generation += 1;
        }
        Ok(())
    }

    /// `UPDATE sessions SET binding_epoch = binding_epoch + 1 ...
    /// RETURNING binding_epoch`.
    async fn mint_binding_epoch(&self, id: SessionId) -> Result<u64, MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        let row = db.sessions.get_mut(&id).ok_or(MetaError::NotFound)?;
        row.binding_epoch += 1;
        Ok(row.binding_epoch as u64)
    }

    async fn current_binding_epoch(&self, id: SessionId) -> Result<u64, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        db.sessions
            .get(&id)
            .map(|r| r.binding_epoch as u64)
            .ok_or(MetaError::NotFound)
    }

    /// Fenced on status='idle' AND current_epoch=$2; true iff the flip
    /// landed. Fires the enqueued placement notify on success.
    async fn enqueue_session_resume(&self, id: SessionId, epoch: i64) -> Result<bool, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some(row) = db.sessions.get_mut(&id) else {
            return Ok(false);
        };
        if row.session.status != SessionState::Idle || row.current_epoch != epoch {
            return Ok(false);
        }
        row.session.status = SessionState::Queued;
        row.queue_origin = Some(QueueOrigin::Resume);
        row.queued_at = Some(now);
        row.session.last_active_at = now;
        db.transition_log.push(super::TransitionLogEntry {
            session: id,
            from: SessionState::Idle,
            to: SessionState::Queued,
            exempt: false,
        });
        drop(db);
        self.notify("placement_changed", "enqueued");
        Ok(true)
    }

    /// `WHERE status='queued' ORDER BY queued_at ASC` (serial-stable).
    async fn list_queued_sessions_fifo(&self) -> Result<Vec<QueuedSession>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        let mut rows: Vec<&SessRow> = db
            .sessions
            .values()
            .filter(|r| r.session.status == SessionState::Queued)
            .collect();
        rows.sort_by_key(|r| (r.queued_at, r.session.id));
        Ok(rows
            .into_iter()
            .map(|r| QueuedSession {
                session: r.session.clone(),
                origin: r.queue_origin.unwrap_or(QueueOrigin::Create),
                mem_budget_mib: r.mem_budget_mib,
                cpu_budget_vcpus: r.cpu_budget_vcpus,
                queued_at: r.queued_at.unwrap_or(r.session.created_at),
            })
            .collect())
    }

    /// tx: pick_host_2d; on fit, `queued -> pending` CAS (`WHERE
    /// status='queued'`); lost race or no fit -> Ok(None).
    async fn place_queued_session(
        &self,
        id: SessionId,
        mem_budget_mib: i64,
        cpu_budget_vcpus: i32,
        candidates: &[HostId],
        affinity_len: usize,
    ) -> Result<Option<HostId>, MetaError> {
        self.gate()?;
        if candidates.is_empty() {
            return Ok(None);
        }
        let now = self.now();
        let mut db = self.db.lock();
        if !db.sessions.contains_key(&id) {
            return Ok(None);
        }
        let Some(host) = Self::pick_host_2d(
            &db,
            candidates,
            affinity_len,
            mem_budget_mib,
            i64::from(cpu_budget_vcpus),
            now,
        ) else {
            return Ok(None);
        };
        let row = db.sessions.get_mut(&id).expect("checked above");
        if row.session.status != SessionState::Queued {
            return Ok(None);
        }
        row.session.status = SessionState::Pending;
        row.session.host_id = Some(host);
        row.session.last_active_at = now;
        db.transition_log.push(super::TransitionLogEntry {
            session: id,
            from: SessionState::Queued,
            to: SessionState::Pending,
            exempt: false,
        });
        Ok(Some(host))
    }

    /// Reserving-state sessions summed per host (pending fresh-only).
    async fn per_host_reserved(
        &self,
    ) -> Result<std::collections::HashMap<HostId, ReservedBudget>, MetaError> {
        self.gate()?;
        let now = self.now();
        let db = self.db.lock();
        let mut out: std::collections::HashMap<HostId, ReservedBudget> =
            std::collections::HashMap::new();
        for row in db.sessions.values() {
            let Some(host) = row.session.host_id else {
                continue;
            };
            let st = row.session.status;
            let counts = reserves(st) && Self::pending_counts_row(&db, row, now);
            if counts {
                let e = out.entry(host).or_default();
                e.mem_mib += row.mem_budget_mib;
                e.vcpus += i64::from(row.cpu_budget_vcpus);
            }
        }
        Ok(out)
    }

    // ================= hosts =================

    /// Mirrors upsert_host's exact column list: hostname, metadata,
    /// capacity, heartbeat, status, capabilities, and COALESCE'd
    /// host_addr. Utilization (allocatable), images/bundles, vcpus,
    /// wire_version, stages_images, and cordoned are HEARTBEAT-only
    /// columns — registration never writes them (a fresh host is
    /// "unmeasured" until its first heartbeat; the conformance suite
    /// caught SimMeta clobbering them from the record).
    async fn upsert_host(&self, host: HostRecord) -> Result<(), MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        match db.hosts.entry(host.id) {
            std::collections::btree_map::Entry::Occupied(mut o) => {
                let prev = o.get_mut();
                prev.hostname = host.hostname;
                prev.cloud_metadata = host.cloud_metadata;
                prev.capacity = host.capacity;
                prev.last_heartbeat_at = host.last_heartbeat_at;
                prev.status = host.status;
                if host.host_addr.is_some() {
                    prev.host_addr = host.host_addr;
                }
                prev.capabilities = host.capabilities;
            }
            std::collections::btree_map::Entry::Vacant(v) => {
                let mut fresh = host;
                // Schema defaults for the heartbeat-only columns.
                fresh.utilization = Default::default();
                fresh.ready_images = Vec::new();
                fresh.current_bundles = Vec::new();
                fresh.cordoned = false;
                fresh.total_vcpus = 0;
                fresh.wire_version = 0;
                fresh.stages_images = false;
                v.insert(fresh);
            }
        }
        drop(db);
        self.notify("placement_changed", "host_upserted");
        Ok(())
    }

    /// `WHERE status IN ('ready','draining') ORDER BY id`.
    async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .hosts
            .values()
            .filter(|h| matches!(h.status, HostStatus::Ready | HostStatus::Draining))
            .cloned()
            .collect())
    }

    async fn set_host_status(&self, id: HostId, status: HostStatus) -> Result<(), MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        let host = db.hosts.get_mut(&id).ok_or(MetaError::NotFound)?;
        host.status = status;
        Ok(())
    }

    /// Heartbeat write: dead is sticky; cordoned untouched.
    async fn touch_host_heartbeat(
        &self,
        id: HostId,
        hb: engram_core::types::host::HostHeartbeat,
    ) -> Result<(), MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let host = db.hosts.get_mut(&id).ok_or(MetaError::NotFound)?;
        if host.status != HostStatus::Dead {
            host.status = hb.status;
        }
        host.capacity = hb.capacity;
        host.utilization = hb.utilization;
        host.ready_images = hb.ready_images;
        host.current_bundles = hb.current_bundles;
        host.total_vcpus = hb.total_vcpus;
        host.wire_version = hb.wire_version;
        host.stages_images = hb.stages_images;
        host.capabilities = hb.capabilities;
        host.last_heartbeat_at = now;
        Ok(())
    }

    async fn set_host_cordoned(&self, id: HostId, cordoned: bool) -> Result<(), MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        let host = db.hosts.get_mut(&id).ok_or(MetaError::NotFound)?;
        host.cordoned = cordoned;
        drop(db);
        if !cordoned {
            self.notify("placement_changed", "host_uncordoned");
        }
        Ok(())
    }

    /// `status='ready' AND (not-cordoned-and-stale OR 10x-stale)`.
    async fn list_stale_hosts(&self, threshold_secs: u64) -> Result<Vec<HostRecord>, MetaError> {
        self.gate()?;
        let now = self.now();
        let db = self.db.lock();
        let t = chrono::Duration::seconds(threshold_secs as i64);
        Ok(db
            .hosts
            .values()
            .filter(|h| h.status == HostStatus::Ready)
            .filter(|h| {
                (!h.cordoned && h.last_heartbeat_at < now - t)
                    || h.last_heartbeat_at < now - (t * 10)
            })
            .cloned()
            .collect())
    }

    /// tx: host -> dead; every non-terminal session on it -> host_lost
    /// with host/sandbox cleared. Returns (id, prev) pairs.
    async fn mark_host_dead_and_orphan_sessions(
        &self,
        host_id: HostId,
    ) -> Result<Vec<(SessionId, SessionState)>, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        if let Some(h) = db.hosts.get_mut(&host_id) {
            h.status = HostStatus::Dead;
        }
        let mut out = Vec::new();
        let mut log = Vec::new();
        for row in db.sessions.values_mut() {
            if row.session.host_id == Some(host_id)
                && !matches!(
                    row.session.status,
                    SessionState::Completed | SessionState::Failed | SessionState::Dead
                )
            {
                let prev = row.session.status;
                row.session.host_id = None;
                row.session.sandbox_id = None;
                row.session.status = SessionState::HostLost;
                row.session.last_active_at = now;
                log.push(super::TransitionLogEntry {
                    session: row.session.id,
                    from: prev,
                    to: SessionState::HostLost,
                    exempt: true,
                });
                out.push((row.session.id, prev));
            }
        }
        db.transition_log.append(&mut log);
        Ok(out)
    }

    async fn host_status(&self, host_id: HostId) -> Result<Option<HostStatus>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db.hosts.get(&host_id).map(|h| h.status))
    }

    /// Issue #722: activity refresh without a transition.
    async fn touch_session_activity(&self, session_id: SessionId) -> Result<(), MetaError> {
        self.gate()?;
        let now = self.now();
        if let Some(r) = self.db.lock().sessions.get_mut(&session_id) {
            r.session.last_active_at = now;
        }
        Ok(())
    }

    /// Insert-or-stale-takeover, exactly the 0106 SQL.
    async fn try_acquire_dead_host_lease(
        &self,
        host_id: HostId,
        claimant: &str,
        stale_after: Duration,
    ) -> Result<bool, MetaError> {
        self.gate()?;
        let now = self.now();
        let cutoff = now
            - chrono::Duration::from_std(stale_after)
                .unwrap_or_else(|_| chrono::Duration::seconds(180));
        let mut db = self.db.lock();
        match db.dead_host_inflight.get(&host_id) {
            Some((_, claimed_at)) if *claimed_at >= cutoff => Ok(false),
            _ => {
                db.dead_host_inflight
                    .insert(host_id, (claimant.to_string(), now));
                Ok(true)
            }
        }
    }

    async fn release_dead_host_lease(
        &self,
        host_id: HostId,
        claimant: &str,
    ) -> Result<(), MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        if db
            .dead_host_inflight
            .get(&host_id)
            .is_some_and(|(by, _)| by == claimant)
        {
            db.dead_host_inflight.remove(&host_id);
        }
        Ok(())
    }

    async fn notify_host_dead(&self, host_id: HostId) -> Result<(), MetaError> {
        self.gate()?;
        self.notify("host_dead", host_id.to_string());
        Ok(())
    }

    async fn put_session_runtime_spec(
        &self,
        session_id: SessionId,
        spec: &engram_core::types::runtime_spec::RuntimeSpec,
    ) -> Result<(), MetaError> {
        self.gate()?;
        self.db
            .lock()
            .runtime_specs
            .insert(session_id, spec.clone());
        Ok(())
    }

    // ================= snapshots =================

    /// Upsert + durable-head advance (monotonic by created_at, gated on
    /// `recoverable`; a demote re-points to the newest recoverable
    /// sibling) + chunk_generation bump, one tx. Returns was-inserted.
    async fn record_snapshot(&self, snap: SnapshotRecord) -> Result<bool, MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        let inserted = !db.snapshots.contains_key(&snap.id);
        let (id, session_id, recoverable, created_at) =
            (snap.id, snap.session_id, snap.recoverable, snap.created_at);
        match db.snapshots.entry(id) {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert(snap);
            }
            std::collections::btree_map::Entry::Occupied(mut o) => {
                // ON CONFLICT: refresh manifests/recoverable/aux;
                // events_cursor and fc_snapshot_version COALESCE-keep.
                let prev = o.get_mut();
                let keep_cursor = snap.events_cursor.or(prev.events_cursor);
                let keep_fc = snap
                    .fc_snapshot_version
                    .clone()
                    .or(prev.fc_snapshot_version.clone());
                *prev = snap;
                prev.events_cursor = keep_cursor;
                prev.fc_snapshot_version = keep_fc;
            }
        }
        db.chunk_generation += 1;
        if let Some(sid) = session_id {
            if recoverable {
                let advance = match db.sessions.get(&sid).and_then(|r| r.durable_head) {
                    None => true,
                    Some(head) => db
                        .snapshots
                        .get(&head)
                        .map(|h| created_at >= h.created_at)
                        .unwrap_or(true),
                };
                if advance {
                    if let Some(row) = db.sessions.get_mut(&sid) {
                        row.durable_head = Some(id);
                    }
                }
            } else if db.sessions.get(&sid).and_then(|r| r.durable_head) == Some(id) {
                // Demoted the current head: re-point to newest
                // still-recoverable sibling (or None).
                let new_head = db
                    .snapshots
                    .values()
                    .filter(|s| s.session_id == Some(sid) && s.recoverable && s.id != id)
                    .max_by_key(|s| (s.created_at, s.id))
                    .map(|s| s.id);
                if let Some(row) = db.sessions.get_mut(&sid) {
                    row.durable_head = new_head;
                }
            }
        }
        Ok(inserted)
    }

    async fn list_snapshots_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SnapshotRecord>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        let mut rows: Vec<SnapshotRecord> = db
            .snapshots
            .values()
            .filter(|s| s.session_id == Some(session_id))
            .cloned()
            .collect();
        rows.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
        Ok(rows)
    }

    async fn latest_snapshot_for_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SnapshotRecord>, MetaError> {
        Ok(self
            .list_snapshots_for_session(session_id)
            .await?
            .into_iter()
            .next())
    }

    // ================= session events =================

    /// CTE mirror: atomically allocate next_event_idx from the session
    /// row (missing session -> NotFound), stamp recovery_epoch, insert,
    /// pg_notify('session_events', {session_id, idx}).
    async fn append_session_event(
        &self,
        session_id: SessionId,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<i64, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let row = db
            .sessions
            .get_mut(&session_id)
            .ok_or(MetaError::NotFound)?;
        let idx = row.next_event_idx;
        row.next_event_idx += 1;
        row.last_event_at = Some(now);
        row.updated_at = now;
        let recovery_epoch = row.recovery_epoch;
        db.session_events
            .entry(session_id)
            .or_default()
            .push(PersistedEvent {
                idx,
                kind: kind.to_string(),
                payload,
                created_at: now,
                recovery_epoch,
                rewound_at: None,
            });
        drop(db);
        self.notify(
            "session_events",
            format!("{{\"session_id\":\"{session_id}\",\"idx\":{idx}}}"),
        );
        Ok(idx)
    }

    async fn list_session_events_since(
        &self,
        session_id: SessionId,
        since: i64,
        limit: i64,
    ) -> Result<Vec<PersistedEvent>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .session_events
            .get(&session_id)
            .map(|v| {
                v.iter()
                    .filter(|e| e.idx > since)
                    .take(limit.max(0) as usize)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    // ================= artifacts =================

    async fn insert_artifact(
        &self,
        id: uuid::Uuid,
        session_id: SessionId,
        blob_key: &str,
        media_type: &str,
        size_bytes: i64,
        caption: Option<&str>,
    ) -> Result<(), MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        if db.artifacts.contains_key(&(session_id, id)) {
            return Err(MetaError::Db("sim: artifacts pk collision".into()));
        }
        db.artifacts.insert(
            (session_id, id),
            ArtifactRow {
                id,
                blob_key: blob_key.to_string(),
                media_type: media_type.to_string(),
                size_bytes,
                caption: caption.map(str::to_string),
                created_at: now,
            },
        );
        Ok(())
    }

    async fn get_artifact(
        &self,
        session_id: SessionId,
        id: uuid::Uuid,
    ) -> Result<Option<ArtifactRow>, MetaError> {
        self.gate()?;
        Ok(self.db.lock().artifacts.get(&(session_id, id)).cloned())
    }

    async fn artifact_usage(&self, session_id: SessionId) -> Result<(i64, i64), MetaError> {
        self.gate()?;
        let db = self.db.lock();
        let mut n = 0i64;
        let mut total = 0i64;
        for ((sid, _), a) in db.artifacts.iter() {
            if *sid == session_id {
                n += 1;
                total += a.size_bytes;
            }
        }
        Ok((n, total))
    }

    // ================= registry credentials =================

    async fn upsert_registry_credential(&self, cred: RegistryCredential) -> Result<(), MetaError> {
        self.gate()?;
        self.db
            .lock()
            .registry_credentials
            .insert(cred.registry_host.clone(), cred);
        Ok(())
    }

    async fn list_registry_credentials(&self) -> Result<Vec<RegistryCredential>, MetaError> {
        self.gate()?;
        Ok(self
            .db
            .lock()
            .registry_credentials
            .values()
            .cloned()
            .collect())
    }

    async fn registry_credential_for_host(
        &self,
        registry_host: &str,
    ) -> Result<Option<RegistryCredential>, MetaError> {
        self.gate()?;
        Ok(self
            .db
            .lock()
            .registry_credentials
            .get(registry_host)
            .cloned())
    }

    async fn delete_registry_credential(&self, registry_host: &str) -> Result<(), MetaError> {
        self.gate()?;
        self.db
            .lock()
            .registry_credentials
            .remove(registry_host)
            .map(|_| ())
            .ok_or(MetaError::NotFound)
    }

    // ================= enabled images =================

    /// Upsert; re-enable always clears soft_deleted_at; bumps
    /// chunk_generation; notifies enabled_image_changed.
    async fn upsert_enabled_image(&self, image: EnabledImage) -> Result<(), MetaError> {
        self.gate()?;
        let uri = image.image_uri.clone();
        let mut db = self.db.lock();
        db.enabled_images.insert(uri.clone(), (image, None));
        db.chunk_generation += 1;
        drop(db);
        self.notify("enabled_image_changed", uri);
        Ok(())
    }

    async fn list_enabled_images(&self) -> Result<Vec<EnabledImage>, MetaError> {
        self.gate()?;
        Ok(self
            .db
            .lock()
            .enabled_images
            .values()
            .filter(|(_, deleted)| deleted.is_none())
            .map(|(img, _)| img.clone())
            .collect())
    }

    async fn get_enabled_image(&self, image_uri: &str) -> Result<Option<EnabledImage>, MetaError> {
        self.gate()?;
        Ok(self
            .db
            .lock()
            .enabled_images
            .get(image_uri)
            .filter(|(_, deleted)| deleted.is_none())
            .map(|(img, _)| img.clone()))
    }

    /// The resume-path lookup: sees soft-deleted rows too.
    async fn get_enabled_image_any(
        &self,
        image_uri: &str,
    ) -> Result<Option<EnabledImage>, MetaError> {
        self.gate()?;
        Ok(self
            .db
            .lock()
            .enabled_images
            .get(image_uri)
            .map(|(img, _)| img.clone()))
    }

    /// Guarded tx: absent -> NotFound; already deleted -> AlreadyDisabled;
    /// blocked by live sessions on the image (up to 16, oldest first);
    /// else stamp soft_deleted_at + bump generation + notify.
    async fn soft_delete_enabled_image(
        &self,
        image_uri: &str,
    ) -> Result<DisableEnabledImageOutcome, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some((_, deleted)) = db.enabled_images.get(image_uri) else {
            return Err(MetaError::NotFound);
        };
        if deleted.is_some() {
            return Ok(DisableEnabledImageOutcome::AlreadyDisabled);
        }
        let mut blockers: Vec<(SessionId, DateTime<Utc>, String)> = db
            .sessions
            .values()
            .filter(|r| {
                r.session.image.as_str() == image_uri
                    && matches!(
                        r.session.status,
                        SessionState::Pending
                            | SessionState::Created
                            | SessionState::Active
                            | SessionState::Evacuating
                    )
            })
            .map(|r| {
                (
                    r.session.id,
                    r.session.created_at,
                    r.session.status.as_str().to_string(),
                )
            })
            .collect();
        if !blockers.is_empty() {
            blockers.sort_by_key(|(id, at, _)| (*at, *id));
            return Ok(DisableEnabledImageOutcome::Blocked(
                blockers
                    .into_iter()
                    .take(16)
                    .map(|(id, _, st)| (id, st))
                    .collect(),
            ));
        }
        db.enabled_images.get_mut(image_uri).expect("checked").1 = Some(now);
        db.chunk_generation += 1;
        drop(db);
        self.notify("enabled_image_changed", image_uri.to_string());
        Ok(DisableEnabledImageOutcome::Disabled)
    }

    async fn delete_enabled_image(&self, image_uri: &str) -> Result<(), MetaError> {
        self.gate()?;
        let removed = self.db.lock().enabled_images.remove(image_uri).is_some();
        if !removed {
            return Err(MetaError::NotFound);
        }
        self.notify("enabled_image_changed", image_uri.to_string());
        Ok(())
    }

    // ================= session secrets =================

    async fn get_session_secrets(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionSecrets>, MetaError> {
        self.gate()?;
        Ok(self.db.lock().session_secrets.get(&session_id).cloned())
    }

    /// Deliberately idempotent — a missing row is fine.
    async fn delete_session_secrets(&self, session_id: SessionId) -> Result<(), MetaError> {
        self.gate()?;
        self.db.lock().session_secrets.remove(&session_id);
        Ok(())
    }

    // ================= sessions: guarded / fenced / scan extras =========

    /// Reset strikes on `present`; +1 each `missing`; rows crossing
    /// `grace_ticks` are returned and reset to 0 (one tx).
    async fn apply_missing_sandbox_strikes(
        &self,
        present: &[SessionId],
        missing: &[SessionId],
        grace_ticks: i32,
    ) -> Result<Vec<SessionId>, MetaError> {
        self.gate()?;
        if present.is_empty() && missing.is_empty() {
            return Ok(Vec::new());
        }
        let mut db = self.db.lock();
        for id in present {
            if let Some(r) = db.sessions.get_mut(id) {
                r.missing_strikes = 0;
            }
        }
        let mut crossed = Vec::new();
        for id in missing {
            if let Some(r) = db.sessions.get_mut(id) {
                r.missing_strikes += 1;
                if r.missing_strikes >= grace_ticks {
                    crossed.push(*id);
                    r.missing_strikes = 0;
                }
            }
        }
        Ok(crossed)
    }

    /// Every Active+bound session with its newest event (fallback:
    /// created_at) — TTL params unused, classification is caller-side.
    async fn list_idle_scan_candidates(
        &self,
        _soft_ttl_secs: i64,
        _hard_ttl_secs: i64,
    ) -> Result<Vec<engram_core::traits::metadata::IdleScanCandidate>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .sessions
            .values()
            .filter(|r| r.session.status == SessionState::Active && r.session.sandbox_id.is_some())
            .map(|r| {
                let last = db.session_events.get(&r.session.id).and_then(|v| v.last());
                engram_core::traits::metadata::IdleScanCandidate {
                    session_id: r.session.id,
                    sandbox_id: r.session.sandbox_id,
                    host_id: r.session.host_id,
                    last_event_at: last.map(|e| e.created_at).unwrap_or(r.session.created_at),
                    last_event_kind: last.map(|e| e.kind.clone()),
                    shell_pinned_until: r.shell_pinned_until,
                }
            })
            .collect())
    }

    /// Silently no-ops on unknown id (mirrors the row-count-unchecked
    /// UPDATE).
    async fn stamp_shell_pin(
        &self,
        id: SessionId,
        pinned_until: DateTime<Utc>,
    ) -> Result<(), MetaError> {
        self.gate()?;
        if let Some(r) = self.db.lock().sessions.get_mut(&id) {
            r.shell_pinned_until = Some(pinned_until);
        }
        Ok(())
    }

    async fn set_session_park_rung(
        &self,
        id: SessionId,
        rung: i16,
        parked_at: Option<DateTime<Utc>>,
    ) -> Result<(), MetaError> {
        self.gate()?;
        if let Some(r) = self.db.lock().sessions.get_mut(&id) {
            r.session.park_rung = rung;
            r.session.parked_at = parked_at;
        }
        Ok(())
    }

    /// Fenced on current_epoch; false = epoch miss or unknown id.
    async fn fenced_set_session_park_rung(
        &self,
        id: SessionId,
        epoch: i64,
        rung: i16,
        parked_at: Option<DateTime<Utc>>,
    ) -> Result<bool, MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        let Some(r) = db.sessions.get_mut(&id) else {
            return Ok(false);
        };
        if r.current_epoch != epoch {
            return Ok(false);
        }
        r.session.park_rung = rung;
        r.session.parked_at = parked_at;
        Ok(true)
    }

    /// Fence FIRST (mismatch -> Ok(None), silent), then legality
    /// (illegal -> Conflict — a caller bug, not a race).
    async fn fenced_transition_session(
        &self,
        session_id: SessionId,
        epoch: i64,
        to: SessionState,
    ) -> Result<Option<SessionState>, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let row = db
            .sessions
            .get_mut(&session_id)
            .ok_or(MetaError::NotFound)?;
        if row.current_epoch != epoch {
            return Ok(None);
        }
        let current = row.session.status;
        current
            .try_transition_to(to)
            .map_err(|e| MetaError::Conflict(e.to_string()))?;
        row.session.status = to;
        let log_entry = super::TransitionLogEntry {
            session: session_id,
            from: current,
            to,
            exempt: false,
        };
        row.session.last_active_at = now;
        row.updated_at = now;
        if to == SessionState::Evacuating {
            row.evac_attempts = 0;
        }
        if to == SessionState::Evicting {
            row.evict_attempts = 0;
        }
        if to == SessionState::Queued {
            row.queued_at = Some(now);
            row.queue_origin = Some(row.queue_origin.unwrap_or(QueueOrigin::Create));
        }
        db.transition_log.push(log_entry);
        drop(db);
        if reserves(current) && !reserves(to) {
            self.notify("placement_changed", "session_freed");
        }
        Ok(Some(current))
    }

    /// `WHERE id=$1 AND current_epoch=$4`; bool = landed.
    async fn fenced_assign_sandbox(
        &self,
        session_id: SessionId,
        epoch: i64,
        sandbox_id: Option<engram_core::SandboxId>,
        host_id: Option<HostId>,
    ) -> Result<bool, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some(r) = db.sessions.get_mut(&session_id) else {
            return Ok(false);
        };
        if r.current_epoch != epoch {
            return Ok(false);
        }
        r.session.sandbox_id = sandbox_id;
        r.session.host_id = host_id;
        r.session.last_active_at = now;
        r.updated_at = now;
        Ok(true)
    }

    /// CAS on expected sandbox + allowed states; distinct Conflict
    /// messages mirror PostgresStore's two rejection strings.
    async fn assign_session_sandbox_guarded(
        &self,
        id: SessionId,
        sandbox_id: Option<engram_core::SandboxId>,
        expected_current: Option<Option<engram_core::SandboxId>>,
        allowed_states: &[SessionState],
    ) -> Result<(), MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some(r) = db.sessions.get_mut(&id) else {
            return Err(MetaError::NotFound);
        };
        if let Some(expected) = expected_current {
            if r.session.sandbox_id != expected {
                return Err(MetaError::Conflict(format!(
                    "assign_session_sandbox CAS: sandbox_id is {:?}, expected {:?}",
                    r.session.sandbox_id, expected
                )));
            }
        }
        if !allowed_states.is_empty() && !allowed_states.contains(&r.session.status) {
            return Err(MetaError::Conflict(format!(
                "assign_session_sandbox CAS: status is {}, not in {:?}",
                r.session.status.as_str(),
                allowed_states
            )));
        }
        r.session.sandbox_id = sandbox_id;
        r.missing_strikes = 0;
        r.updated_at = now;
        if sandbox_id.is_none() {
            r.session.live_disk_manifest = None;
            db.chunk_generation += 1;
        }
        Ok(())
    }

    // ================= session ops (ADR 0079) =================

    /// Insert (idempotency-guarded) then claim-if-free; a keyed
    /// duplicate in an active state -> Duplicate.
    async fn op_enqueue_and_claim(
        &self,
        session_id: SessionId,
        kind: engram_core::types::session_op::OpKind,
        payload: serde_json::Value,
        idempotency_key: Option<&str>,
        claimed_by: &str,
    ) -> Result<engram_core::types::session_op::EnqueueOutcome, MetaError> {
        use engram_core::types::session_op::{EnqueueOutcome, OpState};
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        if let Some(key) = idempotency_key {
            let dup = db.session_ops.values().any(|o| {
                o.session_id == session_id
                    && o.kind == kind
                    && o.idempotency_key.as_deref() == Some(key)
                    && matches!(o.state, OpState::Queued | OpState::Running)
            });
            if dup {
                return Ok(EnqueueOutcome::Duplicate);
            }
        }
        let id = db.next_serial() as i64;
        let mut row = super::OpRow {
            id,
            session_id,
            kind,
            payload,
            state: OpState::Queued,
            step: None,
            epoch: None,
            attempts: 0,
            not_before: None,
            idempotency_key: idempotency_key.map(str::to_string),
            claimed_by: None,
            claimed_at: None,
            heartbeat_at: None,
            error: None,
            created_at: now,
            finished_at: None,
        };
        let running = db
            .session_ops
            .values()
            .any(|o| o.session_id == session_id && o.state == OpState::Running);
        let older_queued = db
            .session_ops
            .values()
            .any(|o| o.session_id == session_id && o.state == OpState::Queued && o.id < id);
        let claimable = !running && !older_queued;
        let outcome = if claimable {
            let sess = db
                .sessions
                .get_mut(&session_id)
                .ok_or(MetaError::NotFound)?;
            sess.current_epoch += 1;
            let epoch = sess.current_epoch;
            row.state = OpState::Running;
            row.epoch = Some(epoch);
            row.claimed_by = Some(claimed_by.to_string());
            row.claimed_at = Some(now);
            row.heartbeat_at = Some(now);
            row.attempts = 1;
            EnqueueOutcome::Claimed(op_row_to_domain(&row))
        } else {
            EnqueueOutcome::Queued(op_row_to_domain(&row))
        };
        db.session_ops.insert(id, row);
        drop(db);
        self.notify("session_ops", session_id.to_string());
        Ok(outcome)
    }

    /// All-or-nothing: insert+claim only if NO other op (queued or
    /// running) exists for the session; else nothing persists.
    async fn op_enqueue_and_claim_exclusive(
        &self,
        session_id: SessionId,
        kind: engram_core::types::session_op::OpKind,
        payload: serde_json::Value,
        claimed_by: &str,
    ) -> Result<Option<engram_core::types::session_op::SessionOp>, MetaError> {
        use engram_core::types::session_op::OpState;
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let busy = db.session_ops.values().any(|o| {
            o.session_id == session_id && matches!(o.state, OpState::Queued | OpState::Running)
        });
        if busy {
            return Ok(None);
        }
        let Some(sess) = db.sessions.get_mut(&session_id) else {
            return Err(MetaError::NotFound);
        };
        sess.current_epoch += 1;
        let epoch = sess.current_epoch;
        let id = db.next_serial() as i64;
        let row = super::OpRow {
            id,
            session_id,
            kind,
            payload,
            state: OpState::Running,
            step: None,
            epoch: Some(epoch),
            attempts: 1,
            not_before: None,
            idempotency_key: None,
            claimed_by: Some(claimed_by.to_string()),
            claimed_at: Some(now),
            heartbeat_at: Some(now),
            error: None,
            created_at: now,
            finished_at: None,
        };
        let op = op_row_to_domain(&row);
        db.session_ops.insert(id, row);
        drop(db);
        self.notify("session_ops", session_id.to_string());
        Ok(Some(op))
    }

    /// Claim the due FIFO head if nothing is running (no epoch burned
    /// otherwise). No notify (mirror).
    async fn op_claim_head(
        &self,
        session_id: SessionId,
        claimed_by: &str,
    ) -> Result<Option<engram_core::types::session_op::SessionOp>, MetaError> {
        use engram_core::types::session_op::OpState;
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let running = db
            .session_ops
            .values()
            .any(|o| o.session_id == session_id && o.state == OpState::Running);
        if running {
            return Ok(None);
        }
        let head = db
            .session_ops
            .values()
            .filter(|o| {
                o.session_id == session_id
                    && o.state == OpState::Queued
                    && o.not_before.is_none_or(|nb| nb <= now)
            })
            .map(|o| o.id)
            .min();
        let Some(op_id) = head else {
            return Ok(None);
        };
        let sess = db
            .sessions
            .get_mut(&session_id)
            .ok_or(MetaError::NotFound)?;
        sess.current_epoch += 1;
        let epoch = sess.current_epoch;
        let row = db.session_ops.get_mut(&op_id).expect("head exists");
        row.state = OpState::Running;
        row.epoch = Some(epoch);
        row.claimed_by = Some(claimed_by.to_string());
        row.claimed_at = Some(now);
        row.heartbeat_at = Some(now);
        row.attempts += 1;
        Ok(Some(op_row_to_domain(row)))
    }

    /// DISTINCT session_ids with a due queued op and nothing running.
    async fn op_due_sessions(&self) -> Result<Vec<SessionId>, MetaError> {
        use engram_core::types::session_op::OpState;
        self.gate()?;
        let now = self.now();
        let db = self.db.lock();
        let mut out = std::collections::BTreeSet::new();
        for o in db.session_ops.values() {
            if o.state == OpState::Queued && o.not_before.is_none_or(|nb| nb <= now) {
                let running = db
                    .session_ops
                    .values()
                    .any(|r| r.session_id == o.session_id && r.state == OpState::Running);
                if !running {
                    out.insert(o.session_id);
                }
            }
        }
        Ok(out.into_iter().collect())
    }

    async fn op_record_step(&self, op_id: i64, epoch: i64, step: &str) -> Result<bool, MetaError> {
        use engram_core::types::session_op::OpState;
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some(o) = db.session_ops.get_mut(&op_id) else {
            return Ok(false);
        };
        if o.epoch != Some(epoch) || o.state != OpState::Running {
            return Ok(false);
        }
        o.step = Some(step.to_string());
        o.heartbeat_at = Some(now);
        Ok(true)
    }

    /// Deliberately does NOT touch `step`.
    async fn op_heartbeat(&self, op_id: i64, epoch: i64) -> Result<bool, MetaError> {
        use engram_core::types::session_op::OpState;
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some(o) = db.session_ops.get_mut(&op_id) else {
            return Ok(false);
        };
        if o.epoch != Some(epoch) || o.state != OpState::Running {
            return Ok(false);
        }
        o.heartbeat_at = Some(now);
        Ok(true)
    }

    async fn op_finish(
        &self,
        op_id: i64,
        epoch: i64,
        state: engram_core::types::session_op::OpState,
        error: Option<&str>,
    ) -> Result<bool, MetaError> {
        use engram_core::types::session_op::OpState;
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some(o) = db.session_ops.get_mut(&op_id) else {
            return Ok(false);
        };
        if o.epoch != Some(epoch) || o.state != OpState::Running {
            return Ok(false);
        }
        o.state = state;
        o.error = error.map(str::to_string);
        o.finished_at = Some(now);
        Ok(true)
    }

    /// Back to queued with backoff; attempts and step PRESERVED (the
    /// next claimer resumes idempotent-from-step); epoch/claim cleared.
    async fn op_requeue_with_backoff(
        &self,
        op_id: i64,
        epoch: i64,
        backoff: std::time::Duration,
        error: &str,
    ) -> Result<bool, MetaError> {
        use engram_core::types::session_op::OpState;
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some(o) = db.session_ops.get_mut(&op_id) else {
            return Ok(false);
        };
        if o.epoch != Some(epoch) || o.state != OpState::Running {
            return Ok(false);
        }
        o.state = OpState::Queued;
        o.not_before = Some(
            now + chrono::Duration::from_std(backoff).unwrap_or_else(|_| chrono::Duration::zero()),
        );
        o.error = Some(error.to_string());
        o.epoch = None;
        o.claimed_by = None;
        o.heartbeat_at = None;
        Ok(true)
    }

    /// Bulk-cancel every queued row of the kind.
    async fn op_cancel_queued(
        &self,
        session_id: SessionId,
        kind: engram_core::types::session_op::OpKind,
    ) -> Result<bool, MetaError> {
        use engram_core::types::session_op::OpState;
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let mut any = false;
        for o in db.session_ops.values_mut() {
            if o.session_id == session_id && o.kind == kind && o.state == OpState::Queued {
                o.state = OpState::Cancelled;
                o.finished_at = Some(now);
                any = true;
            }
        }
        Ok(any)
    }

    /// Pull queued rows' not_before forward to now; notify if any moved.
    async fn op_wake_queued_kind(
        &self,
        session_id: SessionId,
        kind: engram_core::types::session_op::OpKind,
    ) -> Result<u64, MetaError> {
        use engram_core::types::session_op::OpState;
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let mut woken = 0u64;
        for o in db.session_ops.values_mut() {
            if o.session_id == session_id
                && o.kind == kind
                && o.state == OpState::Queued
                && o.not_before.is_some_and(|nb| nb > now)
            {
                o.not_before = Some(now);
                woken += 1;
            }
        }
        drop(db);
        if woken > 0 {
            self.notify("session_ops", session_id.to_string());
        }
        Ok(woken)
    }

    async fn op_request_cancel_running(
        &self,
        session_id: SessionId,
        kind: engram_core::types::session_op::OpKind,
    ) -> Result<bool, MetaError> {
        use engram_core::types::session_op::OpState;
        self.gate()?;
        let mut db = self.db.lock();
        let mut any = false;
        for o in db.session_ops.values_mut() {
            if o.session_id == session_id && o.kind == kind && o.state == OpState::Running {
                o.payload["_cancel"] = serde_json::Value::Bool(true);
                any = true;
            }
        }
        Ok(any)
    }

    async fn op_cancel_requested(&self, op_id: i64) -> Result<bool, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .session_ops
            .get(&op_id)
            .and_then(|o| o.payload.get("_cancel"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    }

    async fn op_running_for(
        &self,
        session_id: SessionId,
    ) -> Result<Option<engram_core::types::session_op::SessionOp>, MetaError> {
        use engram_core::types::session_op::OpState;
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .session_ops
            .values()
            .find(|o| o.session_id == session_id && o.state == OpState::Running)
            .map(op_row_to_domain))
    }

    async fn op_get(
        &self,
        op_id: i64,
    ) -> Result<Option<engram_core::types::session_op::SessionOp>, MetaError> {
        self.gate()?;
        Ok(self.db.lock().session_ops.get(&op_id).map(op_row_to_domain))
    }

    /// Only cancels while still queued.
    async fn op_cancel_by_id(&self, op_id: i64) -> Result<bool, MetaError> {
        use engram_core::types::session_op::OpState;
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some(o) = db.session_ops.get_mut(&op_id) else {
            return Ok(false);
        };
        if o.state != OpState::Queued {
            return Ok(false);
        }
        o.state = OpState::Cancelled;
        o.finished_at = Some(now);
        Ok(true)
    }

    async fn op_pending_exists(
        &self,
        session_id: SessionId,
        kind: engram_core::types::session_op::OpKind,
    ) -> Result<bool, MetaError> {
        use engram_core::types::session_op::OpState;
        self.gate()?;
        let db = self.db.lock();
        Ok(db.session_ops.values().any(|o| {
            o.session_id == session_id
                && o.kind == kind
                && matches!(o.state, OpState::Queued | OpState::Running)
        }))
    }

    /// Stale pending sessions with no create_boot op in flight.
    async fn orphaned_pending_sessions(
        &self,
        older_than: std::time::Duration,
    ) -> Result<Vec<SessionId>, MetaError> {
        use engram_core::types::session_op::{OpKind, OpState};
        self.gate()?;
        let now = self.now();
        let cutoff = now
            - chrono::Duration::from_std(older_than).unwrap_or_else(|_| chrono::Duration::zero());
        let db = self.db.lock();
        Ok(db
            .sessions
            .values()
            .filter(|r| {
                r.session.status == SessionState::Pending && r.session.last_active_at < cutoff
            })
            .filter(|r| {
                !db.session_ops.values().any(|o| {
                    o.session_id == r.session.id
                        && o.kind == OpKind::CreateBoot
                        && matches!(o.state, OpState::Queued | OpState::Running)
                })
            })
            .map(|r| r.session.id)
            .collect())
    }

    /// Reclaim stale-heartbeat running ops: epoch re-bump + re-stamp,
    /// state stays Running (the fence is the epoch, not the state).
    async fn op_reclaim_stale(
        &self,
        stale: std::time::Duration,
        claimed_by: &str,
    ) -> Result<Vec<engram_core::types::session_op::SessionOp>, MetaError> {
        use engram_core::types::session_op::OpState;
        self.gate()?;
        let now = self.now();
        let cutoff =
            now - chrono::Duration::from_std(stale).unwrap_or_else(|_| chrono::Duration::zero());
        let mut db = self.db.lock();
        let stale_ids: Vec<i64> = db
            .session_ops
            .values()
            .filter(|o| o.state == OpState::Running && o.heartbeat_at.is_some_and(|h| h < cutoff))
            .map(|o| o.id)
            .collect();
        let mut out = Vec::new();
        for id in stale_ids {
            let session_id = db.session_ops[&id].session_id;
            let Some(sess) = db.sessions.get_mut(&session_id) else {
                continue;
            };
            sess.current_epoch += 1;
            let epoch = sess.current_epoch;
            let o = db.session_ops.get_mut(&id).expect("collected above");
            o.epoch = Some(epoch);
            o.claimed_by = Some(claimed_by.to_string());
            o.claimed_at = Some(now);
            o.heartbeat_at = Some(now);
            o.attempts += 1;
            out.push(op_row_to_domain(o));
        }
        Ok(out)
    }

    // ================= outbox =================

    /// ON CONFLICT (prompt_id) DO NOTHING; notify fires unconditionally.
    async fn outbox_enqueue(
        &self,
        row: &engram_core::types::outbox::OutboxRow,
    ) -> Result<(), MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        db.outbox
            .entry(row.prompt_id.clone())
            .or_insert_with(|| row.clone());
        drop(db);
        self.notify("session_outbox", row.session_id.to_string());
        Ok(())
    }

    async fn outbox_due_sessions(&self) -> Result<Vec<SessionId>, MetaError> {
        self.gate()?;
        let now = self.now();
        let db = self.db.lock();
        let mut out = std::collections::BTreeSet::new();
        for r in db.outbox.values() {
            if r.acked_at.is_none() && r.not_before <= now {
                out.insert(r.session_id);
            }
        }
        Ok(out.into_iter().collect())
    }

    /// Oldest unacked due row (created_at ASC).
    async fn outbox_next_due(
        &self,
        session_id: SessionId,
    ) -> Result<Option<engram_core::types::outbox::OutboxRow>, MetaError> {
        self.gate()?;
        let now = self.now();
        let db = self.db.lock();
        Ok(db
            .outbox
            .values()
            .filter(|r| r.session_id == session_id && r.acked_at.is_none() && r.not_before <= now)
            .min_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then(a.prompt_id.cmp(&b.prompt_id))
            })
            .cloned())
    }

    async fn outbox_mark_delivered(
        &self,
        prompt_id: &str,
        ack_timeout: std::time::Duration,
    ) -> Result<(), MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        if let Some(r) = db.outbox.get_mut(prompt_id) {
            if r.acked_at.is_none() {
                r.delivered_at = Some(now);
                r.attempts += 1;
                r.not_before = now
                    + chrono::Duration::from_std(ack_timeout)
                        .unwrap_or_else(|_| chrono::Duration::zero());
            }
        }
        Ok(())
    }

    async fn outbox_defer(
        &self,
        prompt_id: &str,
        delay: std::time::Duration,
    ) -> Result<(), MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        if let Some(r) = db.outbox.get_mut(prompt_id) {
            if r.acked_at.is_none() {
                r.not_before = now
                    + chrono::Duration::from_std(delay)
                        .unwrap_or_else(|_| chrono::Duration::zero());
                r.attempts += 1;
            }
        }
        Ok(())
    }

    async fn outbox_ack(&self, prompt_id: &str) -> Result<bool, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        match db.outbox.get_mut(prompt_id) {
            Some(r) if r.acked_at.is_none() => {
                r.acked_at = Some(now);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    // ================= snapshots: extras =================

    async fn get_snapshot(&self, id: SnapshotId) -> Result<Option<SnapshotRecord>, MetaError> {
        self.gate()?;
        Ok(self.db.lock().snapshots.get(&id).cloned())
    }

    async fn durable_head_snapshot(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SnapshotId>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        db.sessions
            .get(&session_id)
            .map(|r| r.durable_head)
            .ok_or(MetaError::NotFound)
    }

    /// Fence gate first (mismatch -> Ok(false), nothing written), then
    /// the exact record_snapshot semantics.
    async fn fenced_record_snapshot(
        &self,
        snap: SnapshotRecord,
        epoch: i64,
    ) -> Result<bool, MetaError> {
        self.gate()?;
        {
            let db = self.db.lock();
            let Some(sid) = snap.session_id else {
                return Err(MetaError::Conflict(
                    "fenced_record_snapshot requires a session_id".into(),
                ));
            };
            let Some(row) = db.sessions.get(&sid) else {
                return Err(MetaError::NotFound);
            };
            if row.current_epoch != epoch {
                return Ok(false);
            }
        }
        self.record_snapshot(snap).await?;
        Ok(true)
    }

    /// Age-based prune keeping each session's newest row regardless of
    /// age; bumps chunk_generation; returns deleted ids.
    async fn prune_session_snapshots(
        &self,
        retention: chrono::Duration,
    ) -> Result<Vec<SnapshotId>, MetaError> {
        self.gate()?;
        let now = self.now();
        let cutoff = now - retention;
        let mut db = self.db.lock();
        let mut newest: std::collections::BTreeMap<SessionId, (DateTime<Utc>, SnapshotId)> =
            std::collections::BTreeMap::new();
        for s in db.snapshots.values() {
            if let Some(sid) = s.session_id {
                let e = newest.entry(sid).or_insert((s.created_at, s.id));
                if (s.created_at, s.id) > *e {
                    *e = (s.created_at, s.id);
                }
            }
        }
        let keep: std::collections::BTreeSet<SnapshotId> =
            newest.values().map(|(_, id)| *id).collect();
        let doomed: Vec<SnapshotId> = db
            .snapshots
            .values()
            .filter(|s| s.session_id.is_some() && s.created_at < cutoff && !keep.contains(&s.id))
            .map(|s| s.id)
            .collect();
        for id in &doomed {
            db.snapshots.remove(id);
        }
        db.chunk_generation += 1;
        Ok(doomed)
    }

    /// Base rows (session_id NULL) older than grace and not pinned by
    /// an enabled image.
    async fn prune_orphan_base_snapshots(
        &self,
        grace: chrono::Duration,
    ) -> Result<Vec<SnapshotId>, MetaError> {
        self.gate()?;
        let now = self.now();
        let cutoff = now - grace;
        let mut db = self.db.lock();
        let pinned: std::collections::BTreeSet<SnapshotId> = db
            .enabled_images
            .values()
            .filter_map(|(img, _)| img.base_snapshot_id)
            .collect();
        let doomed: Vec<SnapshotId> = db
            .snapshots
            .values()
            .filter(|s| s.session_id.is_none() && s.created_at < cutoff && !pinned.contains(&s.id))
            .map(|s| s.id)
            .collect();
        for id in &doomed {
            db.snapshots.remove(id);
        }
        db.chunk_generation += 1;
        Ok(doomed)
    }

    async fn latest_event_idx_at_or_before(
        &self,
        sid: SessionId,
        at: DateTime<Utc>,
    ) -> Result<Option<i64>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .session_events
            .get(&sid)
            .and_then(|v| v.iter().filter(|e| e.created_at <= at).map(|e| e.idx).max()))
    }

    async fn list_session_events_tail(
        &self,
        session_id: SessionId,
        limit: i64,
    ) -> Result<Vec<PersistedEvent>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .session_events
            .get(&session_id)
            .map(|v| {
                let n = v.len().saturating_sub(limit.max(0) as usize);
                v[n..].to_vec()
            })
            .unwrap_or_default())
    }

    /// Fenced CTE mirror: epoch mismatch -> Ok(None), no row, no idx.
    async fn append_session_event_fenced(
        &self,
        session_id: SessionId,
        epoch: i64,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<Option<i64>, MetaError> {
        self.gate()?;
        {
            let db = self.db.lock();
            match db.sessions.get(&session_id) {
                None => return Ok(None),
                Some(r) if r.current_epoch != epoch => return Ok(None),
                Some(_) => {}
            }
        }
        Ok(Some(
            self.append_session_event(session_id, kind, payload).await?,
        ))
    }

    // ================= gc candidates + generation =================

    async fn chunk_generation(&self) -> Result<u64, MetaError> {
        self.gate()?;
        Ok(self.db.lock().chunk_generation)
    }

    async fn bump_chunk_generation(&self) -> Result<(), MetaError> {
        self.gate()?;
        self.db.lock().chunk_generation += 1;
        Ok(())
    }

    /// Sticky first_seen_at.
    async fn upsert_chunk_gc_candidate(&self, hash: [u8; 32]) -> Result<(), MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        db.chunk_gc
            .entry(hash.to_vec())
            .and_modify(|c| c.last_seen_at = now)
            .or_insert(super::GcCandidate {
                first_seen_at: now,
                last_seen_at: now,
            });
        Ok(())
    }

    async fn list_expired_gc_candidates(
        &self,
        cutoff: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<[u8; 32]>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        let mut rows: Vec<(&Vec<u8>, &super::GcCandidate)> = db
            .chunk_gc
            .iter()
            .filter(|(_, c)| c.first_seen_at < cutoff)
            .collect();
        rows.sort_by_key(|(h, c)| (c.first_seen_at, (*h).clone()));
        Ok(rows
            .into_iter()
            .take(limit.max(0) as usize)
            .map(|(h, _)| <[u8; 32]>::try_from(h.as_slice()).expect("32-byte hash"))
            .collect())
    }

    async fn delete_gc_candidates(&self, hashes: &[[u8; 32]]) -> Result<(), MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        for h in hashes {
            db.chunk_gc.remove(h.as_slice());
        }
        Ok(())
    }

    async fn upsert_bundle_gc_candidate(&self, sha256: &str) -> Result<(), MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        db.bundle_gc
            .entry(sha256.to_string())
            .and_modify(|c| c.last_seen_at = now)
            .or_insert(super::GcCandidate {
                first_seen_at: now,
                last_seen_at: now,
            });
        Ok(())
    }

    async fn list_expired_bundle_gc_candidates(
        &self,
        cutoff: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<String>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        let mut rows: Vec<(&String, &super::GcCandidate)> = db
            .bundle_gc
            .iter()
            .filter(|(_, c)| c.first_seen_at < cutoff)
            .collect();
        rows.sort_by_key(|(k, c)| (c.first_seen_at, (*k).clone()));
        Ok(rows
            .into_iter()
            .take(limit.max(0) as usize)
            .map(|(k, _)| k.clone())
            .collect())
    }

    async fn delete_bundle_gc_candidates(&self, sha256s: &[String]) -> Result<(), MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        for k in sha256s {
            db.bundle_gc.remove(k);
        }
        Ok(())
    }

    async fn upsert_snapshot_blob_gc_candidate(&self, id: SnapshotId) -> Result<(), MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        db.snapshot_blob_gc
            .entry(id)
            .and_modify(|c| c.last_seen_at = now)
            .or_insert(super::GcCandidate {
                first_seen_at: now,
                last_seen_at: now,
            });
        Ok(())
    }

    async fn list_expired_snapshot_blob_gc_candidates(
        &self,
        cutoff: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<SnapshotId>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        let mut rows: Vec<(&SnapshotId, &super::GcCandidate)> = db
            .snapshot_blob_gc
            .iter()
            .filter(|(_, c)| c.first_seen_at < cutoff)
            .collect();
        rows.sort_by_key(|(id, c)| (c.first_seen_at, **id));
        Ok(rows
            .into_iter()
            .take(limit.max(0) as usize)
            .map(|(id, _)| *id)
            .collect())
    }

    async fn delete_snapshot_blob_gc_candidates(
        &self,
        ids: &[SnapshotId],
    ) -> Result<(), MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        for id in ids {
            db.snapshot_blob_gc.remove(id);
        }
        Ok(())
    }

    /// Every snapshot row pins its blob (no filter — base + demoted too).
    async fn snapshot_blob_pin_set(&self) -> Result<Vec<SnapshotId>, MetaError> {
        self.gate()?;
        Ok(self.db.lock().snapshots.keys().copied().collect())
    }

    // ================= slab 4: driver-surface extras =================

    async fn ping(&self) -> Result<(), MetaError> {
        self.gate()
    }

    async fn set_session_suggested_title(
        &self,
        id: SessionId,
        title: &str,
    ) -> Result<(), MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        let r = db.sessions.get_mut(&id).ok_or(MetaError::NotFound)?;
        r.session.suggested_title = Some(title.to_string());
        Ok(())
    }

    async fn get_session_runtime_spec(
        &self,
        session_id: SessionId,
    ) -> Result<Option<engram_core::types::runtime_spec::RuntimeSpec>, MetaError> {
        self.gate()?;
        Ok(self.db.lock().runtime_specs.get(&session_id).cloned())
    }

    /// `WHERE sandbox_id=$1 AND host_id=$2 AND status not terminal`.
    async fn session_owning_sandbox(
        &self,
        host_id: HostId,
        sandbox_id: engram_core::SandboxId,
    ) -> Result<Option<SessionId>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .sessions
            .values()
            .find(|r| {
                r.session.sandbox_id == Some(sandbox_id)
                    && r.session.host_id == Some(host_id)
                    && !matches!(
                        r.session.status,
                        SessionState::Failed | SessionState::Completed | SessionState::Dead
                    )
            })
            .map(|r| r.session.id))
    }

    /// `WHERE sandbox_id=$1 AND host_id IS NOT NULL` (any status).
    async fn host_for_sandbox(
        &self,
        sandbox_id: engram_core::SandboxId,
    ) -> Result<Option<(HostId, SessionState)>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .sessions
            .values()
            .find(|r| r.session.sandbox_id == Some(sandbox_id) && r.session.host_id.is_some())
            .map(|r| (r.session.host_id.expect("filtered"), r.session.status)))
    }

    /// Blind rebind: host+sandbox set, strikes reset. NotFound on 0 rows.
    async fn rebind_session(
        &self,
        id: SessionId,
        host_id: HostId,
        sandbox_id: engram_core::SandboxId,
    ) -> Result<(), MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let r = db.sessions.get_mut(&id).ok_or(MetaError::NotFound)?;
        r.session.host_id = Some(host_id);
        r.session.sandbox_id = Some(sandbox_id);
        r.missing_strikes = 0;
        r.updated_at = now;
        Ok(())
    }

    /// Same CAS predicates as the sandbox-guarded sibling.
    async fn assign_session_host_guarded(
        &self,
        id: SessionId,
        host_id: Option<HostId>,
        expected_current: Option<Option<engram_core::SandboxId>>,
        allowed_states: &[SessionState],
    ) -> Result<(), MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some(r) = db.sessions.get_mut(&id) else {
            return Err(MetaError::NotFound);
        };
        let state_ok = allowed_states.is_empty() || allowed_states.contains(&r.session.status);
        let sandbox_ok = match expected_current {
            None => true,
            Some(expected) => r.session.sandbox_id == expected,
        };
        if !state_ok || !sandbox_ok {
            return Err(MetaError::Conflict(format!(
                "guarded session write rejected for {id}: state/sandbox precondition not met"
            )));
        }
        r.session.host_id = host_id;
        r.updated_at = now;
        Ok(())
    }

    async fn list_evacuating_sessions(&self) -> Result<Vec<(Session, u32)>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .sessions
            .values()
            .filter(|r| r.session.status == SessionState::Evacuating)
            .map(|r| (r.session.clone(), r.evac_attempts.max(0) as u32))
            .collect())
    }

    async fn list_evicting_sessions(&self) -> Result<Vec<(Session, u32)>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .sessions
            .values()
            .filter(|r| r.session.status == SessionState::Evicting)
            .map(|r| (r.session.clone(), r.evict_attempts.max(0) as u32))
            .collect())
    }

    async fn bump_evac_attempts(&self, session_id: SessionId) -> Result<u32, MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        let r = db
            .sessions
            .get_mut(&session_id)
            .ok_or(MetaError::NotFound)?;
        r.evac_attempts += 1;
        Ok(r.evac_attempts.max(0) as u32)
    }

    async fn bump_evict_attempts(&self, session_id: SessionId) -> Result<u32, MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        let r = db
            .sessions
            .get_mut(&session_id)
            .ok_or(MetaError::NotFound)?;
        r.evict_attempts += 1;
        Ok(r.evict_attempts.max(0) as u32)
    }

    /// Active+bound sessions whose newest event (fallback created_at)
    /// is older than `now - idle_for_secs`.
    async fn list_active_sessions_idle_past(
        &self,
        idle_for_secs: i64,
    ) -> Result<Vec<(SessionId, engram_core::SandboxId, DateTime<Utc>)>, MetaError> {
        self.gate()?;
        let now = self.now();
        let cutoff = now - chrono::Duration::seconds(idle_for_secs);
        let db = self.db.lock();
        Ok(db
            .sessions
            .values()
            .filter(|r| r.session.status == SessionState::Active && r.session.sandbox_id.is_some())
            .filter_map(|r| {
                let last = db
                    .session_events
                    .get(&r.session.id)
                    .and_then(|v| v.iter().map(|e| e.created_at).max())
                    .unwrap_or(r.session.created_at);
                (last < cutoff).then(|| (r.session.id, r.session.sandbox_id.expect("bound"), last))
            })
            .collect())
    }

    /// Fenced-by-sandbox manifest publish; stale binding -> DroppedStale.
    async fn update_live_disk_manifest(
        &self,
        session_id: SessionId,
        sandbox_id: engram_core::SandboxId,
        manifest_ref: engram_core::types::manifest::ManifestRef,
    ) -> Result<engram_core::traits::UpdateOutcome, MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        let Some(r) = db.sessions.get_mut(&session_id) else {
            return Ok(engram_core::traits::UpdateOutcome::DroppedStale);
        };
        if r.session.sandbox_id != Some(sandbox_id) {
            return Ok(engram_core::traits::UpdateOutcome::DroppedStale);
        }
        r.session.live_disk_manifest = Some(manifest_ref);
        db.chunk_generation += 1;
        Ok(engram_core::traits::UpdateOutcome::Applied)
    }

    async fn list_live_disk_manifest_ids(&self) -> Result<Vec<uuid::Uuid>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .sessions
            .values()
            .filter_map(|r| r.session.live_disk_manifest.as_ref())
            .map(|m| m.manifest_id)
            .collect())
    }

    /// Queued totals: count + Σ budgets.
    async fn queued_demand(&self) -> Result<engram_core::types::session::QueuedDemand, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        let mut out = engram_core::types::session::QueuedDemand::default();
        for r in db.sessions.values() {
            if r.session.status == SessionState::Queued {
                out.sessions += 1;
                out.mem_mib += r.mem_budget_mib.max(0) as u64;
                out.vcpus += i64::from(r.cpu_budget_vcpus).max(0) as u64;
            }
        }
        Ok(out)
    }

    /// Both timestamps on ONE clock (the store's) — the single-clock
    /// property D3 establishes for PostgresStore.
    async fn prompt_received_seconds_ago(
        &self,
        session_id: SessionId,
        prompt_id: &str,
    ) -> Result<Option<f64>, MetaError> {
        self.gate()?;
        let now = self.now();
        let db = self.db.lock();
        Ok(db.session_events.get(&session_id).and_then(|v| {
            v.iter()
                .filter(|e| {
                    e.kind == "prompt_received"
                        && e.payload.get("prompt_id").and_then(|p| p.as_str()) == Some(prompt_id)
                })
                .map(|e| (now - e.created_at).num_milliseconds() as f64 / 1000.0)
                .next_back()
        }))
    }

    /// Idempotent enqueue + event append, one atomic unit; a keyed
    /// collision with DIFFERENT content is a Conflict.
    async fn append_session_event_and_outbox(
        &self,
        session_id: SessionId,
        kind: &str,
        payload: serde_json::Value,
        row: &engram_core::types::outbox::OutboxRow,
    ) -> Result<i64, MetaError> {
        self.gate()?;
        if row.session_id != session_id {
            return Err(MetaError::Serialization(
                "outbox row session_id mismatch".into(),
            ));
        }
        {
            let mut db = self.db.lock();
            match db.outbox.get(&row.prompt_id) {
                None => {
                    db.outbox.insert(row.prompt_id.clone(), row.clone());
                }
                Some(existing) => {
                    if existing.session_id != row.session_id
                        || existing.kind != row.kind
                        || existing.payload != row.payload
                    {
                        return Err(MetaError::Conflict(format!(
                            "outbox id {} belongs to another command",
                            row.prompt_id
                        )));
                    }
                }
            }
        }
        let idx = self.append_session_event(session_id, kind, payload).await?;
        self.notify("session_outbox", session_id.to_string());
        Ok(idx)
    }

    /// Only mutable pre-delivery.
    async fn outbox_update_prompt_text(
        &self,
        prompt_id: &str,
        text: &str,
    ) -> Result<bool, MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        match db.outbox.get_mut(prompt_id) {
            Some(r)
                if r.kind == engram_core::types::outbox::OutboxKind::Prompt
                    && r.delivered_at.is_none()
                    && r.acked_at.is_none() =>
            {
                r.payload["text"] = serde_json::Value::String(text.to_string());
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn outbox_delete_undelivered(&self, prompt_id: &str) -> Result<bool, MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        let deletable = db
            .outbox
            .get(prompt_id)
            .is_some_and(|r| r.delivered_at.is_none() && r.acked_at.is_none());
        if deletable {
            db.outbox.remove(prompt_id);
        }
        Ok(deletable)
    }

    // ============ session satellites (create-tx sidecars) ============

    async fn bind_session_capabilities(
        &self,
        session_id: SessionId,
        caps: &[Capability],
    ) -> Result<(), MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        let entry = db.session_capabilities.entry(session_id).or_default();
        for c in caps {
            // ON CONFLICT DO NOTHING over the (provider, action,
            // resource) key.
            if !entry.contains(c) {
                entry.push(c.clone());
            }
        }
        Ok(())
    }

    async fn get_session_capabilities(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<Capability>, MetaError> {
        self.gate()?;
        Ok(self
            .db
            .lock()
            .session_capabilities
            .get(&session_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn bind_session_integration_policy(
        &self,
        session_id: SessionId,
        policy_json: &str,
    ) -> Result<(), MetaError> {
        self.gate()?;
        self.db
            .lock()
            .session_integration_policy
            .insert(session_id, policy_json.to_string());
        Ok(())
    }

    async fn get_session_integration_policy(
        &self,
        session_id: SessionId,
    ) -> Result<Option<String>, MetaError> {
        self.gate()?;
        Ok(self
            .db
            .lock()
            .session_integration_policy
            .get(&session_id)
            .cloned())
    }

    /// `SELECT harness FROM sessions WHERE id = $1` — the column
    /// mirrored from runtime_spec.selected_harness at create time.
    async fn get_session_harness(
        &self,
        session_id: SessionId,
    ) -> Result<Option<String>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        if !db.sessions.contains_key(&session_id) {
            return Ok(None);
        }
        Ok(db
            .runtime_specs
            .get(&session_id)
            .and_then(|spec| spec.selected_harness.clone()))
    }

    // ================= panic-not-default stubs =================
    // PG-semantic methods not yet mirrored (capture/enable-job
    // machinery, catalogs, org secrets, broker tokens, teleport,
    // cold base, rewind). Each panics so a newly-driven code path
    // fails loudly at the call site instead of silently observing
    // the trait's empty default. Add the method + a conformance
    // case (ADR 0098 D4) before driving it under simulation.

    async fn begin_enable_job_prestage(
        &self,
        _id: uuid::Uuid,
        _claimant: &str,
        _prestage_ref: serde_json::Value,
    ) -> Result<(), MetaError> {
        panic!("SimMeta: begin_enable_job_prestage not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    /// The bundle-GC pin union: every snapshot's aux_bundles, plus the
    /// (unmodeled) mount/harness catalogs — SimDb has no catalog tables
    /// yet, so those unions are the empty set, faithfully matching a
    /// catalog-less database.
    async fn bundle_pin_set(
        &self,
    ) -> Result<Vec<engram_core::types::sandbox::AuxBundleRef>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        let mut out: Vec<engram_core::types::sandbox::AuxBundleRef> = Vec::new();
        for snap in db.snapshots.values() {
            for b in &snap.aux_bundles {
                if !out
                    .iter()
                    .any(|x| x.drive_id == b.drive_id && x.sha256 == b.sha256)
                {
                    out.push(b.clone());
                }
            }
        }
        Ok(out)
    }

    /// `SELECT id, epoch FROM capture_jobs WHERE host_id=$1 AND stage NOT
    /// IN ('done','failed')` — this host's live capture assignments,
    /// echoed on heartbeat.
    ///
    /// DIVERGENCE (documented, same family as `pick_host_2d` /
    /// `live_enable_work_by_host`): the sim has no `capture_jobs` table,
    /// so this is always empty. Conformance covers only that case.
    async fn capture_assignments_for_host(
        &self,
        _host: HostId,
    ) -> Result<Vec<CaptureJobAssignment>, MetaError> {
        self.gate()?;
        Ok(Vec::new())
    }

    async fn claim_enable_jobs(
        &self,
        _claimant: &str,
        _lease_secs: u32,
        _limit: u32,
    ) -> Result<Vec<EnableJob>, MetaError> {
        panic!("SimMeta: claim_enable_jobs not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn cold_base_fc_version_changed(
        &self,
        _disk_manifest: &str,
        _current_fc_version: &str,
    ) -> Result<bool, MetaError> {
        panic!("SimMeta: cold_base_fc_version_changed not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn cold_base_manifest_refs(
        &self,
    ) -> Result<Vec<engram_core::types::manifest::ManifestRef>, MetaError> {
        panic!("SimMeta: cold_base_manifest_refs not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    /// `SELECT snapshot_id FROM cold_bases` — the GC pin source.
    async fn cold_base_snapshot_ids(&self) -> Result<Vec<SnapshotId>, MetaError> {
        self.gate()?;
        Ok(self.db.lock().cold_bases.iter().copied().collect())
    }

    /// `SELECT count(*) FROM chunk_gc_candidates` — the fleet view's
    /// dirty-chunk backlog gauge.
    async fn count_gc_candidates(&self) -> Result<u64, MetaError> {
        self.gate()?;
        Ok(self.db.lock().chunk_gc.len() as u64)
    }

    async fn create_or_get_enable_job(
        &self,
        _image_uri: &str,
        _manifest_digest: Option<&str>,
        // The full image config this enable will capture under (ADR
        // 0080). Rides the job and is stamped onto the enabled_images
        // row only when the job reaches `ready` — capture-affecting
        // edits stay invisible to session-create until the new base
        // snapshot actually exists. Carried from the triggering
        // request (enable/update) or inherited from the existing row
        // (refresh).
        _image_config: &engram_core::types::image::ImageConfig,
    ) -> Result<EnableJob, MetaError> {
        panic!("SimMeta: create_or_get_enable_job not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn create_or_get_enable_job_with_options(
        &self,
        _image_uri: &str,
        _manifest_digest: Option<&str>,
        _image_config: &engram_core::types::image::ImageConfig,
        _force_recapture: bool,
    ) -> Result<EnableJob, MetaError> {
        panic!("SimMeta: create_or_get_enable_job_with_options not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn delete_broker_token(&self, _id: SessionId) -> Result<(), MetaError> {
        panic!("SimMeta: delete_broker_token not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn delete_host(&self, _id: HostId) -> Result<DeleteHostOutcome, MetaError> {
        panic!(
            "SimMeta: delete_host not implemented — add it plus a conformance case (ADR 0098 D4)"
        )
    }

    async fn delete_org_secret(&self, _name: &str) -> Result<bool, MetaError> {
        panic!("SimMeta: delete_org_secret not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn expire_capture_job_stages(
        &self,
        _budgets: &[(CaptureJobStage, std::time::Duration)],
    ) -> Result<Vec<CaptureJobRow>, MetaError> {
        panic!("SimMeta: expire_capture_job_stages not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn find_enabled_image_by_content(
        &self,
        _disk_manifest: ManifestRef,
        _resources: &engram_core::types::image::ResourceHints,
    ) -> Result<Option<EnabledImage>, MetaError> {
        panic!("SimMeta: find_enabled_image_by_content not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn get_broker_token(
        &self,
        _id: SessionId,
    ) -> Result<Option<engram_core::types::registry::SessionBrokerToken>, MetaError> {
        panic!("SimMeta: get_broker_token not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn get_capture_job(&self, _id: CaptureJobId) -> Result<Option<CaptureJobRow>, MetaError> {
        panic!("SimMeta: get_capture_job not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn get_cold_base(&self, _content_key: &str) -> Result<Option<ColdBaseRow>, MetaError> {
        panic!(
            "SimMeta: get_cold_base not implemented — add it plus a conformance case (ADR 0098 D4)"
        )
    }

    async fn get_enable_job(&self, _id: uuid::Uuid) -> Result<Option<EnableJob>, MetaError> {
        panic!("SimMeta: get_enable_job not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn get_harness_by_name(
        &self,
        _name: &str,
    ) -> Result<Option<engram_core::types::CatalogHarness>, MetaError> {
        panic!("SimMeta: get_harness_by_name not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn get_org_secret_sealed(
        &self,
        _name: &str,
    ) -> Result<Option<engram_core::types::org_secret::SealedOrgSecret>, MetaError> {
        panic!("SimMeta: get_org_secret_sealed not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn get_skill_by_name(
        &self,
        _name: &str,
    ) -> Result<Option<engram_core::types::CatalogSkill>, MetaError> {
        panic!("SimMeta: get_skill_by_name not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn get_teleport_target(
        &self,
        _id: SessionId,
    ) -> Result<Option<(HostId, Option<chrono::DateTime<chrono::Utc>>)>, MetaError> {
        panic!("SimMeta: get_teleport_target not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn hosts_with_live_capture_jobs(
        &self,
    ) -> Result<std::collections::HashSet<HostId>, MetaError> {
        panic!("SimMeta: hosts_with_live_capture_jobs not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn insert_broker_token(
        &self,
        _token: engram_core::types::registry::SessionBrokerToken,
    ) -> Result<bool, MetaError> {
        panic!("SimMeta: insert_broker_token not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn insert_capture_job(&self, _row: NewCaptureJob) -> Result<CaptureJobRow, MetaError> {
        panic!("SimMeta: insert_capture_job not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn latest_capture_job_for_enable(
        &self,
        _enable_job_id: uuid::Uuid,
    ) -> Result<Option<CaptureJobRow>, MetaError> {
        panic!("SimMeta: latest_capture_job_for_enable not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn list_active_assignments_with_budgets_on_host(
        &self,
        _host_id: HostId,
    ) -> Result<Vec<SandboxAssignment>, MetaError> {
        panic!("SimMeta: list_active_assignments_with_budgets_on_host not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    /// The reconcile pass query: Active + bound sessions on `host_id`.
    async fn list_active_sandbox_assignments_on_host(
        &self,
        host_id: HostId,
    ) -> Result<Vec<(SessionId, SandboxId)>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .sessions
            .values()
            .filter(|r| {
                r.session.host_id == Some(host_id)
                    && r.session.status == SessionState::Active
                    && r.session.sandbox_id.is_some()
            })
            .map(|r| (r.session.id, r.session.sandbox_id.expect("filtered")))
            .collect())
    }

    async fn list_active_sandboxes_on_host_with_disk_manifest(
        &self,
        _host_id: HostId,
    ) -> Result<Vec<(SessionId, SandboxId, Option<ManifestRef>)>, MetaError> {
        panic!("SimMeta: list_active_sandboxes_on_host_with_disk_manifest not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn list_enable_jobs(&self, _limit: u32) -> Result<Vec<EnableJob>, MetaError> {
        panic!("SimMeta: list_enable_jobs not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn list_enabled_image_base_snapshot_disk_manifests(
        &self,
    ) -> Result<Vec<ManifestRef>, MetaError> {
        panic!("SimMeta: list_enabled_image_base_snapshot_disk_manifests not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn list_enabled_image_base_snapshot_memory_manifests(
        &self,
    ) -> Result<Vec<ManifestRef>, MetaError> {
        panic!("SimMeta: list_enabled_image_base_snapshot_memory_manifests not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn list_enabled_image_disk_manifest_ids(&self) -> Result<Vec<ManifestRef>, MetaError> {
        panic!("SimMeta: list_enabled_image_disk_manifest_ids not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn list_gc_candidates(
        &self,
        _limit: i64,
        _before: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<GcCandidateRow>, MetaError> {
        panic!("SimMeta: list_gc_candidates not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn list_harnesses(&self) -> Result<Vec<engram_core::types::CatalogHarness>, MetaError> {
        panic!("SimMeta: list_harnesses not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn list_live_memory_manifest_ids(&self) -> Result<Vec<uuid::Uuid>, MetaError> {
        panic!("SimMeta: list_live_memory_manifest_ids not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn list_live_session_disk_manifest_ids(&self) -> Result<Vec<ManifestRef>, MetaError> {
        panic!("SimMeta: list_live_session_disk_manifest_ids not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn list_org_secrets(
        &self,
    ) -> Result<Vec<engram_core::types::org_secret::OrgSecret>, MetaError> {
        panic!("SimMeta: list_org_secrets not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    /// `SELECT prestage_ref FROM enable_jobs WHERE state='prestaging'` —
    /// the images a host should prefetch, echoed to it on heartbeat.
    ///
    /// DIVERGENCE (documented, same family as `pick_host_2d` /
    /// `live_enable_work_by_host`): the sim has no `enable_jobs` table, so
    /// this is always the empty set. Conformance covers only that case.
    async fn list_prestaging_refs(&self) -> Result<Vec<serde_json::Value>, MetaError> {
        self.gate()?;
        Ok(Vec::new())
    }

    async fn list_recoverable_snapshot_disk_manifests(
        &self,
    ) -> Result<Vec<ManifestRef>, MetaError> {
        panic!("SimMeta: list_recoverable_snapshot_disk_manifests not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn list_recoverable_snapshot_memory_manifests(
        &self,
    ) -> Result<Vec<ManifestRef>, MetaError> {
        panic!("SimMeta: list_recoverable_snapshot_memory_manifests not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn list_skills(&self) -> Result<Vec<engram_core::types::CatalogSkill>, MetaError> {
        panic!(
            "SimMeta: list_skills not implemented — add it plus a conformance case (ADR 0098 D4)"
        )
    }

    async fn list_waiting_capture_jobs(&self) -> Result<Vec<CaptureJobRow>, MetaError> {
        panic!("SimMeta: list_waiting_capture_jobs not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    /// Live materialize + capture work aggregated by host (the fleet
    /// view's per-host busy overlay). PostgresStore sums `enable_jobs`
    /// (fresh-claimed `materializing`) and `capture_jobs` (non-terminal
    /// stages) by host.
    ///
    /// DIVERGENCE (documented, same family as `pick_host_2d`): the sim
    /// models neither table, so this is always the empty map — a store
    /// with no enable/capture jobs. Conformance covers only that empty
    /// case; scenarios must not create enable/capture jobs.
    async fn live_enable_work_by_host(
        &self,
        _materialize_lease: std::time::Duration,
    ) -> Result<std::collections::HashMap<HostId, engram_core::types::LiveEnableWork>, MetaError>
    {
        self.gate()?;
        Ok(std::collections::HashMap::new())
    }

    async fn mirror_capture_progress_to_enable_job(
        &self,
        _enable_job_id: uuid::Uuid,
        _capture_phase: Option<&str>,
        _warm_stage: Option<&str>,
        _output_tail: Option<&str>,
        // ADR 0088 _addendum: the capture timeline (JSON array of
        // `WarmStageRecord`) for `enable_jobs.warm_stages`; `None`
        // keeps the last-known timeline (COALESCE, like the rest).
        _warm_stages: Option<&serde_json::Value>,
    ) -> Result<(), MetaError> {
        panic!("SimMeta: mirror_capture_progress_to_enable_job not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn place_capture_job(
        &self,
        _id: CaptureJobId,
        _candidates: &[HostId],
    ) -> Result<Option<CaptureJobRow>, MetaError> {
        panic!("SimMeta: place_capture_job not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    /// Diagnostic twin of `pick_host_2d` (no locking): same eligibility
    /// predicate, same reservation sum, per-host fit verdicts with the
    /// same reason labels (`not_lockable` / `unmeasured` / `ram_full` /
    /// `cpu_full` / `fits_now`).
    async fn placement_no_fit_details(
        &self,
        candidates: &[HostId],
        mem_budget_mib: i64,
        cpu_budget_vcpus: i32,
    ) -> Result<Vec<PlacementNoFit>, MetaError> {
        self.gate()?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let now = self.now();
        let db = self.db.lock();
        let mut reserved: std::collections::BTreeMap<HostId, (i64, i64)> = Default::default();
        for row in db.sessions.values() {
            let Some(host) = row.session.host_id else {
                continue;
            };
            let st = row.session.status;
            let counts = st.reserves_host_memory() && Self::pending_counts_row(&db, row, now);
            if counts {
                let e = reserved.entry(host).or_default();
                e.0 += row.mem_budget_mib;
                e.1 += i64::from(row.cpu_budget_vcpus);
            }
        }
        Ok(candidates
            .iter()
            .map(|id| {
                let eligible = db.hosts.get(id).filter(|h| {
                    matches!(h.status, HostStatus::Ready | HostStatus::Draining) && !h.cordoned
                });
                let Some(h) = eligible else {
                    return PlacementNoFit {
                        host_id: *id,
                        reason: "not_lockable",
                        free_mib: 0,
                        free_vcpus: 0,
                    };
                };
                let (res_mib, res_vcpus) = reserved.get(id).copied().unwrap_or((0, 0));
                let alloc = h.utilization.allocatable_mib as i64;
                let cpu_budget = engram_core::types::host::host_cpu_budget(h.total_vcpus);
                let free_vcpus = if cpu_budget > 0 {
                    cpu_budget - res_vcpus
                } else {
                    i64::MAX
                };
                let free_mib = alloc - res_mib;
                let reason = if alloc <= 0 {
                    "unmeasured"
                } else if free_mib < mem_budget_mib {
                    "ram_full"
                } else if free_vcpus < i64::from(cpu_budget_vcpus) {
                    "cpu_full"
                } else {
                    "fits_now"
                };
                PlacementNoFit {
                    host_id: *id,
                    reason,
                    free_mib,
                    free_vcpus,
                }
            })
            .collect())
    }

    async fn reassign_capture_job(
        &self,
        _id: CaptureJobId,
        _expected_epoch: i64,
        _candidates: &[HostId],
    ) -> Result<Option<CaptureJobRow>, MetaError> {
        panic!("SimMeta: reassign_capture_job not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn rebind_session_guarded(
        &self,
        _id: SessionId,
        _host_id: HostId,
        _sandbox_id: SandboxId,
        _expected_current: Option<Option<SandboxId>>,
        _allowed_states: &[SessionState],
    ) -> Result<(), MetaError> {
        panic!("SimMeta: rebind_session_guarded not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn record_capture_job_report(
        &self,
        _report: &CaptureJobReport,
    ) -> Result<bool, MetaError> {
        panic!("SimMeta: record_capture_job_report not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn record_enable_job_failure(
        &self,
        _id: uuid::Uuid,
        _claimant: &str,
        _error: &str,
        _max_attempts: u32,
        _force_terminal: bool,
    ) -> Result<(u32, EnableJobState), MetaError> {
        panic!("SimMeta: record_enable_job_failure not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn redrive_failed_capture_job(
        &self,
        _id: CaptureJobId,
        _expected_epoch: i64,
        _candidates: &[HostId],
        _max_attempts: u32,
    ) -> Result<Option<CaptureJobRow>, MetaError> {
        panic!("SimMeta: redrive_failed_capture_job not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn register_harness(
        &self,
        _reg: engram_core::types::HarnessRegistration<'_>,
    ) -> Result<engram_core::types::CatalogHarness, MetaError> {
        panic!("SimMeta: register_harness not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn register_skill(
        &self,
        _owner: &str,
        _name: &str,
        _description: &str,
        _sha256: &str,
        _mount_json: &str,
        _size_bytes: i64,
    ) -> Result<engram_core::types::CatalogSkill, MetaError> {
        panic!("SimMeta: register_skill not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn release_enable_job_claim(
        &self,
        _id: uuid::Uuid,
        _claimant: &str,
    ) -> Result<bool, MetaError> {
        panic!("SimMeta: release_enable_job_claim not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn retry_enable_job(&self, _id: uuid::Uuid) -> Result<EnableJob, MetaError> {
        panic!("SimMeta: retry_enable_job not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn rewind_session_to_cursor(
        &self,
        _session_id: SessionId,
        _events_cursor: i64,
    ) -> Result<engram_core::types::event::RewindSummary, MetaError> {
        panic!("SimMeta: rewind_session_to_cursor not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn set_enable_job_materialize_host(
        &self,
        _id: uuid::Uuid,
        _claimant: &str,
        _host: HostId,
    ) -> Result<(), MetaError> {
        panic!("SimMeta: set_enable_job_materialize_host not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn set_enable_job_prestage_hosts(
        &self,
        _id: uuid::Uuid,
        _claimant: &str,
        _outcomes: serde_json::Value,
    ) -> Result<(), MetaError> {
        panic!("SimMeta: set_enable_job_prestage_hosts not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn set_enable_job_reuse_outcome(
        &self,
        _enable_job_id: uuid::Uuid,
        _outcome: &str,
    ) -> Result<(), MetaError> {
        panic!("SimMeta: set_enable_job_reuse_outcome not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn set_enable_job_state(
        &self,
        _id: uuid::Uuid,
        _claimant: &str,
        _state: EnableJobState,
    ) -> Result<(), MetaError> {
        panic!("SimMeta: set_enable_job_state not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn set_teleport_target(
        &self,
        _id: SessionId,
        _target: Option<HostId>,
    ) -> Result<(), MetaError> {
        panic!("SimMeta: set_teleport_target not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    /// `SELECT count(*), coalesce(sum(size_bytes),0) FROM snapshots` —
    /// the fleet view's storage aggregate over ALL snapshot rows (session
    /// captures AND template/base snapshots).
    async fn snapshot_totals(&self) -> Result<SnapshotTotals, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        let count = db.snapshots.len() as u64;
        let total_bytes = db.snapshots.values().map(|s| s.size_bytes).sum();
        Ok(SnapshotTotals { count, total_bytes })
    }

    async fn soft_delete_harness(&self, _name: &str) -> Result<bool, MetaError> {
        panic!("SimMeta: soft_delete_harness not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn soft_delete_skill(&self, _name: &str) -> Result<bool, MetaError> {
        panic!("SimMeta: soft_delete_skill not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn update_enable_job_materialize_progress(
        &self,
        _id: uuid::Uuid,
        _claimant: &str,
        _progress: &engram_core::types::MaterializeProgress,
        _stages: &[engram_core::types::WarmStageRecord],
    ) -> Result<(), MetaError> {
        panic!("SimMeta: update_enable_job_materialize_progress not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn update_enable_job_progress(
        &self,
        _id: uuid::Uuid,
        _claimant: &str,
        _chunks_done: u32,
        _chunks_total: Option<u32>,
    ) -> Result<(), MetaError> {
        panic!("SimMeta: update_enable_job_progress not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    /// ADR 0080 cheap-edit path: `UPDATE enabled_images SET image_config,
    /// updated_at WHERE image_uri = $1 AND soft_deleted_at IS NULL`. A
    /// missing OR soft-deleted row affects 0 rows -> NotFound (editing a
    /// disabled image is a re-enable's job). Same `enabled_image_changed`
    /// notify as `upsert_enabled_image`.
    async fn update_enabled_image_config(
        &self,
        image_uri: &str,
        config: &engram_core::types::image::ImageConfig,
    ) -> Result<(), MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some((image, deleted)) = db.enabled_images.get_mut(image_uri) else {
            return Err(MetaError::NotFound);
        };
        if deleted.is_some() {
            // soft_deleted_at IS NULL guard: the UPDATE matches 0 rows.
            return Err(MetaError::NotFound);
        }
        image.image_config = config.clone();
        image.updated_at = Some(now);
        drop(db);
        self.notify("enabled_image_changed", image_uri.to_string());
        Ok(())
    }

    async fn upsert_cold_base(&self, _row: ColdBaseRow) -> Result<(), MetaError> {
        panic!("SimMeta: upsert_cold_base not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn upsert_org_secret(
        &self,
        _sealed: engram_core::types::org_secret::SealedOrgSecret,
    ) -> Result<engram_core::types::org_secret::OrgSecret, MetaError> {
        panic!("SimMeta: upsert_org_secret not implemented — add it plus a conformance case (ADR 0098 D4)")
    }
}

/// `OpRow` (with DB-only lease bookkeeping) -> the domain `SessionOp`.
fn op_row_to_domain(row: &super::OpRow) -> engram_core::types::session_op::SessionOp {
    engram_core::types::session_op::SessionOp {
        id: row.id,
        session_id: row.session_id,
        kind: row.kind,
        payload: row.payload.clone(),
        state: row.state,
        step: row.step.clone(),
        epoch: row.epoch,
        attempts: row.attempts,
        not_before: row.not_before,
        idempotency_key: row.idempotency_key.clone(),
        claimed_by: row.claimed_by.clone(),
        error: row.error.clone(),
        created_at: row.created_at,
        finished_at: row.finished_at,
    }
}
