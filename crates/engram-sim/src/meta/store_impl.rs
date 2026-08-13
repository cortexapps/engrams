//! The `MetadataStore` impl. One lock per method; SQL semantics mirrored
//! from `PostgresStore` (see each method's comment for the statement it
//! replicates). Grouped by table family; unimplemented PG-semantic
//! methods panic via `sim_unimplemented!` at the bottom.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use engram_core::traits::metadata::{
    CreateDisposition, DisableEnabledImageOutcome, ExecLifecycleEventKind, ExecOutputStream,
    GcCandidateRow, MetadataStore, PlacementNoFit, SessionCreateWriteSet, SnapshotTotals,
};
use engram_core::types::capability::Capability;
use engram_core::types::capture_job::{
    CaptureJobAssignment, CaptureJobReport, CaptureJobRow, CaptureJobStage, CaptureTerminalReport,
    ColdBaseRow, NewCaptureJob,
};
use engram_core::types::event::{ArtifactRow, EventCursor, PersistedEvent};
use engram_core::types::host::{HostRecord, HostStatus, ReservedBudget};
use engram_core::types::ids::CaptureJobId;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::registry::{EnableJob, EnableJobState};
use engram_core::types::registry::{EnabledImage, RegistryCredential, SessionSecrets};
use engram_core::types::session::SandboxAssignment;
use engram_core::types::session::{
    BindingDisposition, DeleteHostOutcome, QueueOrigin, QueuedSession, Session, SessionSpec,
    SessionState,
};
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::SandboxId;
use engram_core::{HostId, MetaError, SessionId, SnapshotId};

use super::{EnableJobRow, SessRow, SimDb, SimMetadataStore};

/// States that hold a host-memory reservation — mirrors
/// `PostgresStore::host_memory_reserving_states()` /
/// `SessionState::reserves_host_memory`.
///
/// R3 (#722): ONE reservation authority — a session in a reserving state
/// (including a `pending` pinned to a host) reserves its budget
/// UNCONDITIONALLY, for exactly as long as it is in that state. There is NO
/// crash-orphan wall-age / live-op exclusion: an aged pending physically
/// holds its slot until it LEAVES the reserving state, and the ADR 0079
/// pending-orphan backstop reclaims a true orphan by a real
/// `pending → failed` transition (the sole reclaimer). This mirrors the
/// PostgresStore SQL twin and equals the unconditional
/// placement-accounting oracle by construction.
fn reserves(state: SessionState) -> bool {
    state.reserves_host_memory()
}

impl SimMetadataStore {
    /// Oracle-only helper for engram-dst — reuses the EXACT reserve-path
    /// predicate so the capacity-aware Queued-at-quiescence check cannot
    /// drift from the store.
    pub fn oracle_pick_any_host(
        &self,
        mem_budget_mib: i64,
        cpu_budget_vcpus: i64,
    ) -> Option<HostId> {
        let db = self.db.lock();
        let candidates: Vec<HostId> = db.hosts.keys().copied().collect();
        Self::pick_host_2d(&db, &candidates, 0, mem_budget_mib, cpu_budget_vcpus)
    }

    /// Mirror of `pick_host_2d` + `choose_placement_host`: candidates
    /// filtered to ready|draining and not cordoned; reservations summed
    /// over reserving-state sessions (R3 #722: every `pending` counts
    /// unconditionally — no crash-orphan exclusion); best-fit = smallest
    /// allocatable-minus-reserved RAM that fits both budgets, affinity tier
    /// (`candidates[..affinity_len]`) first, then the rest, then any
    /// unmeasured host (allocatable == 0) as a last resort.
    ///
    fn pick_host_2d(
        db: &SimDb,
        candidates: &[HostId],
        affinity_len: usize,
        mem_budget_mib: i64,
        cpu_budget_vcpus: i64,
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
            let counts = reserves(st);
            if counts {
                let e = reserved.entry(host).or_default();
                e.0 += row.mem_budget_mib;
                e.1 += i64::from(row.cpu_budget_vcpus);
            }
        }
        for row in db.capture_jobs.values() {
            if !row.stage.is_terminal() {
                if let Some(host) = row.host_id {
                    let e = reserved.entry(host).or_default();
                    e.0 += row.mem_budget_mib;
                    e.1 += i64::from(row.cpu_budget_vcpus);
                }
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
                    last_event_at: None,
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
        let db = self.db.lock();
        // PG twin: every non-terminal state except `host_lost` — the
        // reserving set (so a new resident state like ADR 0101 C's
        // `parked` can't drift out) plus `idle`.
        Ok(db
            .sessions
            .values()
            .filter(|r| {
                r.session.status.reserves_host_memory() || r.session.status == SessionState::Idle
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
        if ws.oauth_binding.as_ref().is_some_and(|binding| {
            db.oauth_credentials
                .get(&binding.key)
                .is_none_or(|credential| credential.revoked_at.is_some())
        }) {
            return Err(MetaError::Conflict(
                "session OAuth binding requires a live credential".into(),
            ));
        }
        let picked = Self::pick_host_2d(
            &db,
            candidates,
            affinity_len,
            ws.mem_budget_mib,
            i64::from(ws.cpu_budget_vcpus),
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
                    last_event_at: None,
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
        if let Some(binding) = ws.oauth_binding {
            db.session_oauth_bindings.insert(ws.session_id, binding);
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
        disposition: BindingDisposition,
    ) -> Result<SessionState, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let row = db.sessions.get_mut(&id).ok_or(MetaError::NotFound)?;
        let current = row.session.status;
        current
            .try_transition_to(target)
            .map_err(|e| MetaError::Conflict(e.to_string()))?;
        // #896 / ADR 0090 addendum: disposition legality under the same
        // db lock — the PG twin's row-lock check.
        if !target.binding_disposition_legal(row.session.sandbox_id.is_some(), disposition) {
            return Err(MetaError::Conflict(format!(
                "illegal binding disposition {disposition:?} into {} on a bound row",
                target.as_str()
            )));
        }
        row.session.status = target;
        if matches!(disposition, BindingDisposition::Detach) {
            row.session.sandbox_id = None;
        }
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

    /// Terminal transition (ADR 0015 M2): pick the terminal target for the
    /// current state and drive `transition_session` to it; `None` when the
    /// session is already terminal. PostgresStore does NOT override the trait
    /// default either — both stores compose `get_session` +
    /// `transition_session`. Written out explicitly here (rather than
    /// inheriting the default) so the conformance suite exercises the
    /// composition against Sim's own row-locked `transition_session`, and so a
    /// future PG-side divergence surfaces as a Sim gap, not a silent drift
    /// (ADR 0098 D4).
    async fn terminate_session(
        &self,
        id: SessionId,
    ) -> Result<Option<(SessionState, SessionState)>, MetaError> {
        let session = self.get_session(id).await?;
        let Some(target) = session.status.terminal_target() else {
            return Ok(None);
        };
        // Terminal rows must not own (mirrors the trait default): Detach.
        let prev = self
            .transition_session(id, target, BindingDisposition::Detach)
            .await?;
        Ok(Some((prev, target)))
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

    async fn enqueue_evacuating_session_resume(
        &self,
        id: SessionId,
        epoch: i64,
    ) -> Result<bool, MetaError> {
        // #800: fenced `evacuating → queued` (resume origin) — the sim twin
        // of the PG CAS, gated on `status='evacuating'` + epoch.
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some(row) = db.sessions.get_mut(&id) else {
            return Ok(false);
        };
        if row.session.status != SessionState::Evacuating || row.current_epoch != epoch {
            return Ok(false);
        }
        row.session.status = SessionState::Queued;
        row.queue_origin = Some(QueueOrigin::Resume);
        row.queued_at = Some(now);
        row.session.last_active_at = now;
        db.transition_log.push(super::TransitionLogEntry {
            session: id,
            from: SessionState::Evacuating,
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
        let db = self.db.lock();
        let mut out: std::collections::HashMap<HostId, ReservedBudget> =
            std::collections::HashMap::new();
        for row in db.sessions.values() {
            let Some(host) = row.session.host_id else {
                continue;
            };
            let st = row.session.status;
            let counts = reserves(st);
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
                // ADR 0116 A-D3, exactly the 0115 ON CONFLICT arm: a
                // register REPLACES the lease deadline (successor presence
                // ends a handoff early — no GREATEST here, unlike the
                // heartbeat renew), flips Active, bumps the generation
                // fence. A None target leaves the lease untouched.
                if let Some(until) = host.lease_expires_at {
                    prev.lease_expires_at = Some(until);
                    prev.lease_state = engram_core::types::host::HostLeaseState::Active;
                    prev.lease_epoch += 1;
                }
            }
            std::collections::btree_map::Entry::Vacant(v) => {
                let mut fresh = host;
                // Schema defaults for the heartbeat-only columns.
                fresh.utilization = Default::default();
                fresh.ready_images = Vec::new();
                fresh.current_bundles = Vec::new();
                fresh.sandbox_bundles = Vec::new();
                fresh.cordoned = false;
                fresh.total_vcpus = 0;
                fresh.wire_version = 0;
                fresh.stages_images = false;
                // ADR 0116 A-D1, exactly the 0115 INSERT arm.
                fresh.lease_state = if fresh.lease_expires_at.is_some() {
                    engram_core::types::host::HostLeaseState::Active
                } else {
                    engram_core::types::host::HostLeaseState::None
                };
                fresh.lease_epoch = 1;
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

    /// ADR 0068: `hosts.capabilities.fc_snapshot_version` for one host.
    /// PostgresStore does not override the trait default; both stores derive
    /// it from an active-host scan. Written out explicitly (rather than
    /// inheriting the default) so the conformance suite pins Sim's
    /// `list_active_hosts` semantics (`ready|draining`) as the backing lookup
    /// and a future PG divergence surfaces as a Sim gap (ADR 0098 D4).
    async fn fc_snapshot_version_for_host(
        &self,
        host_id: HostId,
    ) -> Result<Option<String>, MetaError> {
        Ok(self
            .list_active_hosts()
            .await?
            .into_iter()
            .find(|h| h.id == host_id)
            .and_then(|h| h.capabilities.fc_snapshot_version))
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
        host.sandbox_bundles = hb.sandbox_bundles;
        host.total_vcpus = hb.total_vcpus;
        host.wire_version = hb.wire_version;
        host.stages_images = hb.stages_images;
        host.capabilities = hb.capabilities;
        host.last_heartbeat_at = now;
        // ADR 0116 A-D3, exactly the 0115 heartbeat-renew CASE arms:
        // GREATEST (a racing predecessor heartbeat never shrinks a handoff
        // deadline); never demote a declared handoff; None skips renewal.
        if let Some(renew) = hb.lease_renew_until {
            host.lease_expires_at = Some(match host.lease_expires_at {
                Some(existing) => existing.max(renew),
                None => renew,
            });
            if host.lease_state != engram_core::types::host::HostLeaseState::Handoff {
                host.lease_state = engram_core::types::host::HostLeaseState::Active;
            }
        }
        Ok(())
    }

    /// ADR 0116 A-D2: exactly the 0115 `begin_host_handoff` UPDATE.
    async fn begin_host_handoff(
        &self,
        id: HostId,
        until: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool, MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        let Some(host) = db.hosts.get_mut(&id) else {
            return Ok(false);
        };
        if host.status == HostStatus::Dead {
            return Ok(false);
        }
        host.lease_state = engram_core::types::host::HostLeaseState::Handoff;
        host.lease_expires_at = Some(match host.lease_expires_at {
            Some(existing) => existing.max(until),
            None => until,
        });
        Ok(true)
    }

    /// ADR 0116 A-D4: `status='ready' AND (lease_expires_at IS NULL OR
    /// lease_expires_at < now)`. NULL = no shield. No cordon multiplier.
    async fn list_lease_expired_hosts(&self) -> Result<Vec<HostRecord>, MetaError> {
        self.gate()?;
        let now = self.now();
        let db = self.db.lock();
        Ok(db
            .hosts
            .values()
            .filter(|h| h.status == HostStatus::Ready)
            .filter(|h| h.lease_expires_at.is_none_or(|expires| expires < now))
            .cloned()
            .collect())
    }

    /// ADR 0116 A-D4: exactly the `renew_host_lease` UPDATE — GREATEST,
    /// `none` promotes to `active`, `handoff` never demoted, dead rows
    /// excluded.
    async fn renew_host_lease(
        &self,
        id: HostId,
        until: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool, MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        let Some(host) = db.hosts.get_mut(&id) else {
            return Ok(false);
        };
        if host.status == HostStatus::Dead {
            return Ok(false);
        }
        host.lease_expires_at = Some(match host.lease_expires_at {
            Some(existing) => existing.max(until),
            None => until,
        });
        if host.lease_state == engram_core::types::host::HostLeaseState::None {
            host.lease_state = engram_core::types::host::HostLeaseState::Active;
        }
        Ok(true)
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

    /// tx: lease re-checked under the lock (renewed ⇒ `Conflict`, ADR
    /// 0116 A-D4); then host -> dead; every non-terminal session on it
    /// -> host_lost with host/sandbox cleared. Returns (id, prev) pairs.
    async fn mark_host_dead_if_lease_expired(
        &self,
        host_id: HostId,
    ) -> Result<Vec<(SessionId, SessionState)>, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        match db.hosts.get_mut(&host_id) {
            None => return Ok(Vec::new()),
            Some(h) if h.status == HostStatus::Dead => return Ok(Vec::new()),
            Some(h) => {
                if h.lease_expires_at.is_some_and(|expires| expires >= now) {
                    return Err(MetaError::Conflict(format!(
                        "host {host_id} lease renewed to {:?}; refusing to mark dead",
                        h.lease_expires_at
                    )));
                }
                h.status = HostStatus::Dead;
            }
        }
        let mut out = Vec::new();
        let mut log = Vec::new();
        let mut tombstones = Vec::new();
        for row in db.sessions.values_mut() {
            if row.session.host_id == Some(host_id)
                && !matches!(
                    row.session.status,
                    SessionState::Completed | SessionState::Failed | SessionState::Dead
                )
            {
                let prev = row.session.status;
                // ADR 0116 A-D5: same-tx tombstone per cleared binding,
                // exactly the PG CTE's `entombed` leg.
                if let Some(sandbox_id) = row.session.sandbox_id {
                    tombstones.push(((host_id, sandbox_id), (Some(row.session.id), now)));
                }
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
        for (key, value) in tombstones {
            db.sandbox_tombstones.entry(key).or_insert(value);
        }
        db.transition_log.append(&mut log);
        Ok(out)
    }

    /// ADR 0116 A-D5: exactly the `record_sandbox_tombstone` INSERT —
    /// idempotent on the (host, sandbox) key.
    async fn record_sandbox_tombstone(
        &self,
        host_id: HostId,
        sandbox_id: engram_core::SandboxId,
        session_id: Option<SessionId>,
    ) -> Result<(), MetaError> {
        self.gate()?;
        let now = self.now();
        self.db
            .lock()
            .sandbox_tombstones
            .entry((host_id, sandbox_id))
            .or_insert((session_id, now));
        Ok(())
    }

    async fn sandbox_tombstones_for_host(
        &self,
        host_id: HostId,
    ) -> Result<Vec<engram_core::SandboxId>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .sandbox_tombstones
            .keys()
            .filter(|(h, _)| *h == host_id)
            .map(|(_, s)| *s)
            .collect())
    }

    /// ADR 0116 A-D5: delete every tombstone for the host whose sandbox
    /// is absent from the reported running set (ack-by-absence).
    async fn ack_sandbox_tombstones_by_absence(
        &self,
        host_id: HostId,
        running: &[engram_core::SandboxId],
    ) -> Result<Vec<engram_core::SandboxId>, MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        let acked: Vec<engram_core::SandboxId> = db
            .sandbox_tombstones
            .keys()
            .filter(|(h, s)| *h == host_id && !running.contains(s))
            .map(|(_, s)| *s)
            .collect();
        for s in &acked {
            db.sandbox_tombstones.remove(&(host_id, *s));
        }
        Ok(acked)
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

    async fn session_exec_event_at(
        &self,
        session_id: SessionId,
        exec_id: &str,
        kind: ExecLifecycleEventKind,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        // `SELECT MIN((payload->>'at')::timestamptz) ... WHERE kind = $2 AND
        //  payload->>'exec_id' = $3 AND rewound_at IS NULL` — the event's OWN
        // `at` stamp (attach time), not the row's created_at (first-frame
        // time), and the dedup sees the live timeline only.
        Ok(db.session_events.get(&session_id).and_then(|events| {
            events
                .iter()
                .filter(|event| {
                    event.rewound_at.is_none()
                        && event.kind == kind.kind_str()
                        && event
                            .payload
                            .get("exec_id")
                            .and_then(serde_json::Value::as_str)
                            == Some(exec_id)
                })
                .filter_map(|event| {
                    event
                        .payload
                        .get("at")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|raw| {
                            chrono::DateTime::parse_from_rfc3339(raw)
                                .ok()
                                .map(|at| at.with_timezone(&chrono::Utc))
                        })
                })
                .min()
        }))
    }

    async fn session_exec_output_high_water(
        &self,
        session_id: SessionId,
        exec_id: &str,
        stream: ExecOutputStream,
    ) -> Result<u64, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        // `SELECT MAX((payload->>'bytes_end')::bigint) ... AND rewound_at IS
        // NULL` — unstamped rows are skipped, matching NULL-ignoring SQL MAX,
        // and tombstoned rows must not hold the mark up.
        Ok(db
            .session_events
            .get(&session_id)
            .map(|events| {
                events
                    .iter()
                    .filter(|event| {
                        event.rewound_at.is_none()
                            && event.kind == stream.kind_str()
                            && event
                                .payload
                                .get("exec_id")
                                .and_then(serde_json::Value::as_str)
                                == Some(exec_id)
                    })
                    .filter_map(|event| {
                        event
                            .payload
                            .get("bytes_end")
                            .and_then(serde_json::Value::as_u64)
                    })
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0))
    }

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
        row.session.last_event_at = Some(now);
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

    /// The sim mirror of PostgresStore's one event-log query. The stored
    /// vec is append-ordered, so ascending idx is its natural order and
    /// `.rev()` is the DESC subquery. The filters must match the SQL
    /// exactly, so both read the kind→field map from engram-core
    /// (`PersistedEvent::tool_name`): a kind that carries NO tool name
    /// passes a tool-name filter untouched, like the SQL `IS NULL` arm.
    async fn list_session_events_window(
        &self,
        session_id: SessionId,
        cursor: EventCursor,
        limit: i64,
        kinds: &[String],
        tool_names: &[String],
    ) -> Result<Vec<PersistedEvent>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        let Some(all) = db.session_events.get(&session_id) else {
            return Ok(Vec::new());
        };
        // A non-positive limit is an empty page, never "unlimited" —
        // same contract PG's `LIMIT 0` gives.
        let limit = limit.max(0) as usize;
        let keep = |e: &PersistedEvent| {
            (kinds.is_empty() || kinds.iter().any(|k| k == &e.kind))
                && (tool_names.is_empty()
                    || e.tool_name()
                        .is_none_or(|name| tool_names.iter().any(|t| t == name)))
        };
        Ok(match cursor {
            EventCursor::After(n) => all
                .iter()
                .filter(|e| e.idx > n && keep(e))
                .take(limit)
                .cloned()
                .collect(),
            EventCursor::Before(n) => {
                // Newest-first, then re-ascend — the page is the LAST
                // `limit` matches below the anchor, in reading order.
                let mut page: Vec<PersistedEvent> = all
                    .iter()
                    .rev()
                    .filter(|e| e.idx < n && keep(e))
                    .take(limit)
                    .cloned()
                    .collect();
                page.reverse();
                page
            }
        })
    }

    /// Phase 1c (ADR 0052): PostgresStore fans one EPHEMERAL live-token delta
    /// out cross-replica via `NOTIFY session_event_deltas` — no row, no idx,
    /// best-effort by contract (a dropped notification costs only live
    /// animation, never correctness; the durable event log is the record).
    /// The simulator models a single logical store with no peer-replica
    /// listener bus, so there is nobody to fan out to — a no-op is the CORRECT
    /// Sim behavior, not a silently-inherited default. Still gated so a
    /// simulated PG-outage window mirrors PG's failing `pg_notify` execute
    /// (ADR 0098 D4).
    async fn notify_session_delta(
        &self,
        _session_id: SessionId,
        _payload: &serde_json::Value,
    ) -> Result<(), MetaError> {
        self.gate()?;
        Ok(())
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
        file_name: Option<&str>,
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
                file_name: file_name.map(str::to_string),
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
        // Fidelity: `enabled_images.base_snapshot_id` is NOT NULL + FK in
        // PG (migration 0038). Accepting `None` here green-lit a scenario
        // PG rejects (caught by the conformance suite's PG half,
        // 2026-07-17) — enforce loudly instead of diverging silently.
        assert!(
            image.base_snapshot_id.is_some()
                && image.base_snapshot_disk_manifest.is_some()
                && image.base_snapshot_memory_manifest.is_some(),
            "SimMeta fidelity: enabled_images.base_snapshot_id (migration 0038) and \
             the base_snapshot_{{disk,memory}}_manifest pair (migration 0043) are \
             NOT NULL in PG — record a base snapshot first and set all three"
        );
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
        disposition: BindingDisposition,
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
        // #896 / ADR 0090 addendum: disposition legality after the fence
        // check, under the same lock (the PG twin's ordering).
        if !to.binding_disposition_legal(row.session.sandbox_id.is_some(), disposition) {
            return Err(MetaError::Conflict(format!(
                "illegal binding disposition {disposition:?} into {} on a bound row",
                to.as_str()
            )));
        }
        row.session.status = to;
        if matches!(disposition, BindingDisposition::Detach) {
            row.session.sandbox_id = None;
        }
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

    /// `fenced_transition_session` + event appends under ONE db lock —
    /// the sim's transaction. Mirrors PostgresStore: fence check before
    /// legality; on success the events land with consecutive indices and
    /// per-event `session_events` notifications; on a stale epoch or an
    /// illegal transition NOTHING lands (no state change, no events, no
    /// notifications — pg_notify only fires on commit).
    async fn fenced_transition_session_with_events(
        &self,
        session_id: SessionId,
        epoch: i64,
        to: SessionState,
        disposition: BindingDisposition,
        events: &[(String, serde_json::Value)],
    ) -> Result<Option<(SessionState, Vec<i64>)>, MetaError> {
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
        // #896 / ADR 0090 addendum: disposition legality after the fence
        // check, under the same lock (the PG twin's ordering).
        if !to.binding_disposition_legal(row.session.sandbox_id.is_some(), disposition) {
            return Err(MetaError::Conflict(format!(
                "illegal binding disposition {disposition:?} into {} on a bound row",
                to.as_str()
            )));
        }
        row.session.status = to;
        row.session.last_active_at = now;
        row.updated_at = now;
        if matches!(disposition, BindingDisposition::Detach) {
            row.session.sandbox_id = None;
        }
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
        let recovery_epoch = row.recovery_epoch;
        let mut indices = Vec::with_capacity(events.len());
        for (kind, payload) in events {
            // Per-iteration re-borrow: the session row and the event log
            // are sibling fields of the same locked db, so the row borrow
            // can't span the event push.
            let row = db
                .sessions
                .get_mut(&session_id)
                .expect("session row present under the same lock");
            let idx = row.next_event_idx;
            row.next_event_idx += 1;
            row.session.last_event_at = Some(now);
            indices.push(idx);
            db.session_events
                .entry(session_id)
                .or_default()
                .push(PersistedEvent {
                    idx,
                    kind: kind.clone(),
                    payload: payload.clone(),
                    created_at: now,
                    recovery_epoch,
                    rewound_at: None,
                });
        }
        db.transition_log.push(super::TransitionLogEntry {
            session: session_id,
            from: current,
            to,
            exempt: false,
        });
        drop(db);
        for idx in &indices {
            self.notify(
                "session_events",
                format!("{{\"session_id\":\"{session_id}\",\"idx\":{idx}}}"),
            );
        }
        if reserves(current) && !reserves(to) {
            self.notify("placement_changed", "session_freed");
        }
        Ok(Some((current, indices)))
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

    /// ADR 0101 C: mirrors PG's `ORDER BY id DESC LIMIT 1` newest-mint
    /// read over `(session, kind)`, any state, any key.
    async fn op_latest_for_kind(
        &self,
        session_id: SessionId,
        kind: engram_core::types::session_op::OpKind,
    ) -> Result<Option<engram_core::types::session_op::SessionOp>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .session_ops
            .values()
            .filter(|o| o.session_id == session_id && o.kind == kind)
            .max_by_key(|o| o.id)
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

    /// Inverse of `outbox_defer`: `not_before := now` for waiting
    /// un-acked rows. Never bumps `attempts` (not a delivery try);
    /// the `not_before > now` predicate keeps it idempotent and off
    /// already-due rows — same semantics as the PG UPDATE.
    async fn outbox_make_due(&self, session_id: SessionId) -> Result<u64, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let mut moved = 0u64;
        for r in db.outbox.values_mut() {
            if r.session_id == session_id && r.acked_at.is_none() && r.not_before > now {
                r.not_before = now;
                moved += 1;
            }
        }
        Ok(moved)
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

    async fn list_host_lost_sessions(&self) -> Result<Vec<Session>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .sessions
            .values()
            .filter(|r| r.session.status == SessionState::HostLost)
            .map(|r| r.session.clone())
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

    async fn list_parked_sessions(&self) -> Result<Vec<Session>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .sessions
            .values()
            .filter(|r| r.session.status == SessionState::Parked)
            .map(|r| r.session.clone())
            .collect())
    }

    /// ADR 0101 C: mirrors the PG single-statement settle — status CAS
    /// + sandbox match + recoverable-row EXISTS, all-or-nothing.
    async fn settle_evicted_session_idle(
        &self,
        session_id: SessionId,
        sandbox_id: engram_core::SandboxId,
        snapshot_id: engram_core::types::SnapshotId,
        events: &[(String, serde_json::Value)],
    ) -> Result<Option<Vec<i64>>, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        // (PG stamps `updated_at`, a column the in-memory `Session`
        // doesn't carry — nothing to mirror here.)
        let row_recoverable = db
            .snapshots
            .get(&snapshot_id)
            .is_some_and(|s| s.session_id == Some(session_id) && s.recoverable);
        if !row_recoverable {
            return Ok(None);
        }
        let Some(r) = db.sessions.get_mut(&session_id) else {
            return Ok(None);
        };
        if r.session.status != SessionState::Evicting || r.session.sandbox_id != Some(sandbox_id) {
            return Ok(None);
        }
        r.session.status = SessionState::Idle;
        r.session.sandbox_id = None;
        // The settle's lifecycle facts land under the same db lock (the
        // sim's transaction) — the settle is CAS-once, so a caller-side
        // append after it had a crash window of permanent loss.
        let recovery_epoch = r.recovery_epoch;
        let mut indices = Vec::with_capacity(events.len());
        for (kind, payload) in events {
            let r = db
                .sessions
                .get_mut(&session_id)
                .expect("session row present under the same lock");
            let idx = r.next_event_idx;
            r.next_event_idx += 1;
            r.session.last_event_at = Some(now);
            indices.push(idx);
            db.session_events
                .entry(session_id)
                .or_default()
                .push(PersistedEvent {
                    idx,
                    kind: kind.clone(),
                    payload: payload.clone(),
                    created_at: now,
                    recovery_epoch,
                    rewound_at: None,
                });
        }
        drop(db);
        for idx in &indices {
            self.notify(
                "session_events",
                format!("{{\"session_id\":\"{session_id}\",\"idx\":{idx}}}"),
            );
        }
        // Parity with the retired D5 Idle flip: `evicting → idle` frees
        // a memory-reserving state's budget — wake the queue scanner.
        self.notify("placement_changed", "session_freed");
        Ok(Some(indices))
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

    async fn append_events_with_outbox_idempotent(
        &self,
        session_id: SessionId,
        events: &[(String, serde_json::Value)],
        row: &engram_core::types::outbox::OutboxRow,
    ) -> Result<Option<Vec<i64>>, MetaError> {
        // Faithful mirror of the PG impl: a retry of the SAME command
        // (existing prompt_id, matching identity) appends NOTHING and
        // returns Ok(None); a prompt_id claimed by a DIFFERENT command
        // is Conflict. Only a fresh insert appends the events.
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
                    // Payload is part of the command identity (review
                    // finding on #993, PG parity): a reused prompt_id
                    // with different text/mode conflicts, never drops.
                    if existing.session_id != row.session_id
                        || existing.kind != row.kind
                        || existing.payload != row.payload
                    {
                        return Err(MetaError::Conflict(format!(
                            "outbox id {} belongs to another command",
                            row.prompt_id
                        )));
                    }
                    return Ok(None);
                }
            }
        }
        let mut idxs = Vec::with_capacity(events.len());
        for (kind, payload) in events {
            idxs.push(
                self.append_session_event(session_id, kind, payload.clone())
                    .await?,
            );
        }
        self.notify("session_outbox", session_id.to_string());
        Ok(Some(idxs))
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

    /// The bundle-GC pin union: every snapshot's aux_bundles ∪ every
    /// live (ready|draining) host's per-sandbox attachments (the ADR 0035 amendment
    /// D2) ∪ every live host's bake stamp, plus the (unmodeled)
    /// mount/harness catalogs — SimDb has no catalog tables yet, so
    /// those unions are the empty set, faithfully matching a
    /// catalog-less database. Sorted by `(drive_id, sha256)` to match
    /// PostgresStore's ordering.
    async fn bundle_pin_set(
        &self,
    ) -> Result<Vec<engram_core::types::sandbox::AuxBundleRef>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        let mut out: Vec<engram_core::types::sandbox::AuxBundleRef> = Vec::new();
        let mut push = |b: &engram_core::types::sandbox::AuxBundleRef| {
            if !out
                .iter()
                .any(|x| x.drive_id == b.drive_id && x.sha256 == b.sha256)
            {
                out.push(b.clone());
            }
        };
        for snap in db.snapshots.values() {
            for b in &snap.aux_bundles {
                push(b);
            }
        }
        for host in db
            .hosts
            .values()
            .filter(|h| matches!(h.status, HostStatus::Ready | HostStatus::Draining))
        {
            for sb in &host.sandbox_bundles {
                for b in &sb.bundles {
                    push(b);
                }
            }
            for b in &host.current_bundles {
                push(b);
            }
        }
        drop(db);
        out.sort_by(|a, b| (&a.drive_id, &a.sha256).cmp(&(&b.drive_id, &b.sha256)));
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
        claimant: &str,
        lease_secs: u32,
        limit: u32,
    ) -> Result<Vec<EnableJob>, MetaError> {
        self.gate()?;
        let now = self.now();
        let lease = chrono::Duration::seconds(i64::from(lease_secs));
        let mut db = self.db.lock();
        let mut eligible: Vec<_> = db
            .enable_jobs
            .values()
            .filter(|row| {
                !row.job.state.is_terminal()
                    && row
                        .claimed_at
                        .is_none_or(|claimed_at| claimed_at < now - lease)
            })
            .map(|row| (row.job.created_at, row.job.id))
            .collect();
        eligible.sort_unstable();
        let mut claimed = Vec::new();
        for (_, id) in eligible.into_iter().take(limit as usize) {
            let row = db.enable_jobs.get_mut(&id).expect("selected row exists");
            row.claimed_by = Some(claimant.to_string());
            row.claimed_at = Some(now);
            row.job.updated_at = now;
            claimed.push(row.job.clone());
        }
        Ok(claimed)
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
        image_uri: &str,
        manifest_digest: Option<&str>,
        // The full image config this enable will capture under (ADR
        // 0080). Rides the job and is stamped onto the enabled_images
        // row only when the job reaches `ready` — capture-affecting
        // edits stay invisible to session-create until the new base
        // snapshot actually exists. Carried from the triggering
        // request (enable/update) or inherited from the existing row
        // (refresh).
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
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        if let Some(existing) = db
            .enable_jobs
            .values()
            .find(|row| row.job.image_uri == image_uri && !row.job.state.is_terminal())
        {
            return Ok(existing.job.clone());
        }
        let job = EnableJob {
            id: self.entropy.uuid(),
            image_uri: image_uri.to_string(),
            manifest_digest: manifest_digest.map(str::to_string),
            state: EnableJobState::Pending,
            chunks_total: None,
            chunks_done: 0,
            attempts: 0,
            error: None,
            image_config: image_config.clone(),
            force_recapture,
            prestage_hosts: serde_json::json!({}),
            capture_phase: None,
            warm_stage: None,
            warm_stage_started_at: None,
            warm_stages: Vec::new(),
            materialize_stages: Vec::new(),
            materialize_host_id: None,
            output_tail: None,
            created_at: now,
            updated_at: now,
        };
        db.enable_jobs.insert(
            job.id,
            EnableJobRow {
                job: job.clone(),
                claimed_by: None,
                claimed_at: None,
            },
        );
        Ok(job)
    }

    async fn delete_broker_token(&self, id: SessionId) -> Result<(), MetaError> {
        self.gate()?;
        self.db.lock().broker_tokens.remove(&id);
        Ok(())
    }

    async fn delete_host(&self, id: HostId) -> Result<DeleteHostOutcome, MetaError> {
        // PG twin: refuse while any RESIDENT session (the reserving set —
        // a `parked` paused-in-place VM included) or non-terminal capture
        // job is bound; otherwise detach stragglers and delete,
        // idempotently.
        self.gate()?;
        let mut db = self.db.lock();
        let bound = db
            .sessions
            .values()
            .filter(|r| r.session.host_id == Some(id) && reserves(r.session.status))
            .count()
            + db.capture_jobs
                .values()
                .filter(|j| j.host_id == Some(id) && !j.stage.is_terminal())
                .count();
        if bound > 0 {
            return Ok(DeleteHostOutcome::SessionsBound(bound as u64));
        }
        for r in db.sessions.values_mut() {
            if r.session.host_id == Some(id) {
                r.session.host_id = None;
            }
        }
        db.hosts.remove(&id);
        Ok(DeleteHostOutcome::Deleted)
    }

    async fn delete_org_secret(&self, _name: &str) -> Result<bool, MetaError> {
        panic!("SimMeta: delete_org_secret not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn expire_capture_job_stages(
        &self,
        budgets: &[(CaptureJobStage, std::time::Duration)],
    ) -> Result<Vec<CaptureJobRow>, MetaError> {
        self.gate()?;
        if budgets.is_empty() {
            return Ok(Vec::new());
        }
        let now = self.now();
        Ok(self
            .db
            .lock()
            .capture_jobs
            .values()
            .filter(|job| !job.stage.is_terminal() && job.host_id.is_some())
            .filter(|job| {
                budgets
                    .iter()
                    .find(|(stage, _)| *stage == job.stage)
                    .is_some_and(|(_, budget)| {
                        now.signed_duration_since(job.last_progress_at)
                            .to_std()
                            .is_ok_and(|age| age > *budget)
                    })
            })
            .cloned()
            .collect())
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
        id: SessionId,
    ) -> Result<Option<engram_core::types::registry::SessionBrokerToken>, MetaError> {
        self.gate()?;
        Ok(self.db.lock().broker_tokens.get(&id).cloned())
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

    async fn get_oauth_credential(
        &self,
        key: &engram_core::types::oauth::OAuthCredentialKey,
    ) -> Result<Option<engram_core::types::oauth::SealedOAuthCredential>, MetaError> {
        self.gate()?;
        Ok(self.db.lock().oauth_credentials.get(key).cloned())
    }

    async fn list_oauth_credentials(
        &self,
        subject_kind: engram_core::types::oauth::OAuthSubjectKind,
        subject_id: Option<&str>,
    ) -> Result<Vec<engram_core::types::oauth::SealedOAuthCredential>, MetaError> {
        self.gate()?;
        Ok(self
            .db
            .lock()
            .oauth_credentials
            .values()
            .filter(|credential| {
                credential.key.subject_kind == subject_kind
                    && subject_id.is_none_or(|id| credential.key.subject_id == id)
            })
            .cloned()
            .collect())
    }

    async fn get_oauth_flow(
        &self,
        id: uuid::Uuid,
    ) -> Result<Option<engram_core::types::oauth::OAuthFlow>, MetaError> {
        self.gate()?;
        Ok(self.db.lock().oauth_flows.get(&id).cloned())
    }

    async fn get_session_oauth_binding(
        &self,
        session_id: SessionId,
    ) -> Result<Option<engram_core::types::oauth::SessionOAuthBinding>, MetaError> {
        self.gate()?;
        Ok(self
            .db
            .lock()
            .session_oauth_bindings
            .get(&session_id)
            .cloned())
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
        self.gate()?;
        Ok(self.db.lock().teleport_targets.get(&_id).copied())
    }

    async fn hosts_with_live_capture_jobs(
        &self,
    ) -> Result<std::collections::HashSet<HostId>, MetaError> {
        panic!("SimMeta: hosts_with_live_capture_jobs not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn insert_broker_token(
        &self,
        token: engram_core::types::registry::SessionBrokerToken,
    ) -> Result<bool, MetaError> {
        // First-writer-wins (PG: ON CONFLICT (session_id) DO NOTHING).
        self.gate()?;
        let mut db = self.db.lock();
        if let std::collections::btree_map::Entry::Vacant(slot) =
            db.broker_tokens.entry(token.session_id)
        {
            slot.insert(token);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn insert_capture_job(&self, row: NewCaptureJob) -> Result<CaptureJobRow, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        if let Some(existing) = db
            .capture_jobs
            .values()
            .find(|job| job.enable_job_id == row.enable_job_id && !job.stage.is_terminal())
        {
            return Ok(existing.clone());
        }
        let job = CaptureJobRow {
            id: CaptureJobId::from(self.entropy.uuid()),
            enable_job_id: row.enable_job_id,
            image_uri: row.image_uri,
            manifest_digest: row.manifest_digest,
            disk_manifest: row.disk_manifest,
            image_config: row.image_config,
            oci_defaults: row.oci_defaults,
            host_id: None,
            mem_budget_mib: row.mem_budget_mib,
            cpu_budget_vcpus: row.cpu_budget_vcpus,
            waiting_since: Some(now),
            epoch: 1,
            stage: CaptureJobStage::Assigned,
            stage_started_at: now,
            stage_progress: None,
            last_progress_at: now,
            attempts: 1,
            retryable: None,
            error: None,
            error_stage: None,
            fc_snapshot_version: None,
            result_bincode: None,
            created_at: now,
            updated_at: now,
        };
        db.capture_jobs.insert(job.id, job.clone());
        Ok(job)
    }

    async fn latest_capture_job_for_enable(
        &self,
        _enable_job_id: uuid::Uuid,
    ) -> Result<Option<CaptureJobRow>, MetaError> {
        panic!("SimMeta: latest_capture_job_for_enable not implemented — add it plus a conformance case (ADR 0098 D4)")
    }

    async fn list_resident_assignments_with_budgets_on_host(
        &self,
        host_id: HostId,
    ) -> Result<Vec<SandboxAssignment>, MetaError> {
        // PG twin: RESIDENT (memory-reserving) + bound sessions on
        // `host_id`, with their reservation budgets (COALESCE NULL → 0).
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .sessions
            .values()
            .filter(|r| {
                r.session.host_id == Some(host_id)
                    && r.session.status.reserves_host_memory()
                    && r.session.sandbox_id.is_some()
            })
            .map(|r| SandboxAssignment {
                session_id: r.session.id,
                sandbox_id: r.session.sandbox_id.expect("filtered"),
                status: r.session.status,
                mem_budget_mib: r.mem_budget_mib,
                cpu_budget_vcpus: r.cpu_budget_vcpus,
            })
            .collect())
    }

    /// The reconcile pass query: Active + bound sessions on `host_id`.
    async fn list_resident_sandbox_assignments_on_host(
        &self,
        host_id: HostId,
    ) -> Result<Vec<(SessionId, SandboxId, SessionState)>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        Ok(db
            .sessions
            .values()
            .filter(|r| {
                r.session.host_id == Some(host_id)
                    && r.session.status.reserves_host_memory()
                    && r.session.sandbox_id.is_some()
            })
            .map(|r| {
                (
                    r.session.id,
                    r.session.sandbox_id.expect("filtered"),
                    r.session.status,
                )
            })
            .collect())
    }

    async fn list_resident_sandboxes_on_host_with_disk_manifest(
        &self,
        host_id: HostId,
    ) -> Result<Vec<(SessionId, SandboxId, Option<ManifestRef>)>, MetaError> {
        // PG twin: sessions in any `reserves_host_memory` state with a
        // sandbox bound on this host (a rung-parked `Evicting` VM is
        // as resident as an `Active` one — session 731df805), each
        // resolved to the effective disk manifest: newer of the live
        // manifest and the latest recoverable snapshot's, same-id →
        // max(version), different id → snapshot wins.
        self.gate()?;
        let db = self.db.lock();
        let mut out = Vec::new();
        for row in db.sessions.values() {
            let s = &row.session;
            if !s.status.reserves_host_memory() {
                continue;
            }
            let (Some(h), Some(sb)) = (s.host_id, s.sandbox_id) else {
                continue;
            };
            if h != host_id {
                continue;
            }
            // Latest recoverable snapshot with a disk manifest
            // (PG: DISTINCT ON (session_id) … ORDER BY created_at DESC).
            let snap = db
                .snapshots
                .values()
                .filter(|snap| {
                    snap.session_id == Some(s.id)
                        && snap.recoverable
                        && snap.disk_manifest.is_some()
                })
                .max_by_key(|snap| snap.created_at)
                .and_then(|snap| snap.disk_manifest);
            let effective = match (s.live_disk_manifest, snap) {
                (None, snap) => snap,
                (Some(l), None) => Some(l),
                (Some(l), Some(sn)) => {
                    if l.manifest_id == sn.manifest_id && l.version > sn.version {
                        Some(l)
                    } else {
                        Some(sn)
                    }
                }
            };
            out.push((s.id, sb, effective));
        }
        Ok(out)
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
        self.gate()?;
        Ok(self
            .db
            .lock()
            .capture_jobs
            .values()
            .filter(|job| !job.stage.is_terminal() && job.host_id.is_none())
            .cloned()
            .collect())
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
        id: CaptureJobId,
        candidates: &[HostId],
    ) -> Result<Option<CaptureJobRow>, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some(current) = db.capture_jobs.get(&id) else {
            return Ok(None);
        };
        if current.stage.is_terminal() {
            return Ok(None);
        }
        if current.host_id.is_some() {
            return Ok(Some(current.clone()));
        }
        let picked = Self::pick_host_2d(
            &db,
            candidates,
            0,
            current.mem_budget_mib,
            i64::from(current.cpu_budget_vcpus),
        );
        let row = db.capture_jobs.get_mut(&id).expect("checked above");
        row.host_id = picked;
        row.updated_at = now;
        if picked.is_some() {
            row.waiting_since = None;
            row.stage_started_at = now;
            row.last_progress_at = now;
        } else if row.waiting_since.is_none() {
            row.waiting_since = Some(now);
        }
        Ok(Some(row.clone()))
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
        let db = self.db.lock();
        let mut reserved: std::collections::BTreeMap<HostId, (i64, i64)> = Default::default();
        for row in db.sessions.values() {
            let Some(host) = row.session.host_id else {
                continue;
            };
            let st = row.session.status;
            let counts = st.reserves_host_memory();
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
        id: SessionId,
        host_id: HostId,
        sandbox_id: SandboxId,
        expected_current: Option<Option<SandboxId>>,
        allowed_states: &[SessionState],
    ) -> Result<(), MetaError> {
        // PG twin (rebind onto a fresh host+sandbox under the same CAS as
        // assign_session_sandbox_guarded, also stamping host_id and — issue
        // #215 — clearing the reconcile strike streak). A guard miss is a
        // Conflict; a missing row is NotFound.
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some(r) = db.sessions.get_mut(&id) else {
            return Err(MetaError::NotFound);
        };
        if let Some(expected) = expected_current {
            if r.session.sandbox_id != expected {
                return Err(MetaError::Conflict(format!(
                    "rebind_session_guarded CAS: sandbox_id is {:?}, expected {:?}",
                    r.session.sandbox_id, expected
                )));
            }
        }
        if !allowed_states.is_empty() && !allowed_states.contains(&r.session.status) {
            return Err(MetaError::Conflict(format!(
                "rebind_session_guarded CAS: status is {}, not in {:?}",
                r.session.status.as_str(),
                allowed_states
            )));
        }
        r.session.host_id = Some(host_id);
        r.session.sandbox_id = Some(sandbox_id);
        r.missing_strikes = 0;
        r.updated_at = now;
        Ok(())
    }

    async fn record_capture_job_report(
        &self,
        report: &CaptureJobReport,
    ) -> Result<bool, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some(row) = db.capture_jobs.get_mut(&report.job_id) else {
            return Ok(false);
        };
        if row.epoch != report.epoch || row.stage.is_terminal() {
            return Ok(false);
        }
        let stage = match &report.terminal {
            Some(CaptureTerminalReport::Done { result_bincode }) => {
                row.result_bincode = Some(result_bincode.clone());
                CaptureJobStage::Done
            }
            Some(CaptureTerminalReport::Failed {
                error,
                error_stage,
                retryable,
            }) => {
                row.error = Some(error.clone());
                row.error_stage = Some(error_stage.clone());
                row.retryable = Some(*retryable);
                CaptureJobStage::Failed
            }
            None => report.stage,
        };
        if row.stage != stage {
            row.stage_started_at = now;
        }
        row.stage = stage;
        row.stage_progress = report.progress.clone();
        row.last_progress_at = now;
        if report.fc_snapshot_version.is_some() {
            row.fc_snapshot_version = report.fc_snapshot_version.clone();
        }
        row.updated_at = now;
        Ok(true)
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
        session_id: SessionId,
        events_cursor: i64,
    ) -> Result<engram_core::types::event::RewindSummary, MetaError> {
        // Faithful mirror of `PostgresStore::rewind_session_to_cursor`
        // (crates/engram-postgres): detect surviving side-effects in the
        // rolled-back span, tombstone every GUEST-HISTORY event past the
        // cursor (a positive list — everything else survives by
        // default), and — only if anything actually rewound — bump the
        // recovery epoch. The guest-history list and the side-effect
        // line text MUST match the SQL exactly; the conformance suite
        // (engram-sim/tests) runs the same scenario against both stores.
        // ADR 0098 D4 conformance rule: this method changed from a panic
        // stub to a real impl in the PR that added `resume_started` to the
        // old exclusion set.
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();

        // The guest-history kinds the rewind MAY touch (mirror the PG
        // positive `kind IN (...)` predicate — keep in lockstep).
        // Everything NOT in this set survives by default: coordinator
        // facts, stable waiting markers, user intent, user input, and
        // any kind added in the future. `agent_message` is handled
        // below with the role predicate (assistant/system = guest
        // history; the user echo = input; no role = survives).
        const GUEST_HISTORY: &[&str] = &[
            "run_started",
            "run_completed",
            "run_interrupted",
            "tool_call_started",
            "tool_call_completed",
            "tool_call_requested",
            "browser_activity",
            "prompt_queued",
            "prompt_edited",
            "prompt_dequeued",
            "prompt_steered",
            "file_changed",
            "title_suggested",
            "integration_asset",
            "exec_started",
            "exec_completed",
            "stdout",
            "stderr",
        ];
        fn is_guest_history(kind: &str, payload: &serde_json::Value) -> bool {
            GUEST_HISTORY.contains(&kind)
                || (kind == "agent_message"
                    && payload.get("role").and_then(|v| v.as_str()) != Some("user")
                    && payload.get("role").and_then(|v| v.as_str()).is_some())
        }

        let (tombstoned, surviving_side_effects) = {
            let Some(events) = db.session_events.get_mut(&session_id) else {
                // No events for this session -> nothing rewinds (parity with
                // the PG zero-rows early return; no epoch bump).
                return Ok(engram_core::types::event::RewindSummary::default());
            };

            // Surviving side-effects: outside-world actions in the
            // rolled-back span the rewind CANNOT undo. Same detection +
            // one-line-each rendering as the SQL.
            let mut surviving = Vec::new();
            for e in events.iter() {
                if e.idx <= events_cursor || e.rewound_at.is_some() {
                    continue;
                }
                match e.kind.as_str() {
                    "integration_asset"
                        if e.payload.get("surface").and_then(|v| v.as_str()) == Some("asset") =>
                    {
                        let provider = e
                            .payload
                            .get("provider")
                            .and_then(|v| v.as_str())
                            .unwrap_or("integration");
                        let asset_kind = e
                            .payload
                            .get("asset_kind")
                            .and_then(|v| v.as_str())
                            .unwrap_or("asset");
                        let detail = e
                            .payload
                            .get("fetchable")
                            .and_then(|f| f.get("url"))
                            .and_then(|v| v.as_str())
                            .or_else(|| {
                                e.payload
                                    .get("data")
                                    .and_then(|d| d.get("url").or_else(|| d.get("title")))
                                    .and_then(|v| v.as_str())
                            });
                        surviving.push(match detail {
                            Some(d) => format!(
                                "A {provider} {asset_kind} was produced and still exists: {d}"
                            ),
                            None => {
                                format!("A {provider} {asset_kind} was produced and still exists")
                            }
                        });
                    }
                    // `file_shared` is user input now — it survives the
                    // rewind, so the event itself stays visible and a
                    // "still exists" note would be redundant (PG parity).
                    _ => {}
                }
            }

            // Tombstone the rolled-back span (audit-preserving) and count it.
            let mut n: u64 = 0;
            for e in events.iter_mut() {
                if e.idx > events_cursor
                    && e.rewound_at.is_none()
                    && is_guest_history(&e.kind, &e.payload)
                {
                    e.rewound_at = Some(now);
                    n += 1;
                }
            }
            (n, surviving)
        };

        if tombstoned == 0 {
            // Checkpoint was already the head, or the only post-cursor rows
            // are excluded coordinator facts: nothing user-visible rewound.
            // Don't bump the epoch (keeps the no-op clean) — PG parity.
            return Ok(engram_core::types::event::RewindSummary::default());
        }

        // Bump the epoch so events appended after this segment carry it.
        let row = db
            .sessions
            .get_mut(&session_id)
            .ok_or(MetaError::NotFound)?;
        row.recovery_epoch += 1;
        row.updated_at = now;
        let recovery_epoch = row.recovery_epoch;

        Ok(engram_core::types::event::RewindSummary {
            rolled_back: tombstoned,
            recovery_epoch,
            through_idx: events_cursor,
            surviving_side_effects,
        })
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
        id: SessionId,
        target: Option<HostId>,
    ) -> Result<(), MetaError> {
        // PG twin: set/clear the pin + its `_set_at` together (issue #214).
        // A no-op for an absent session, like the bare UPDATE.
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        match target {
            Some(h) => {
                db.teleport_targets.insert(id, (h, Some(now)));
            }
            None => {
                db.teleport_targets.remove(&id);
            }
        }
        Ok(())
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

    async fn put_oauth_credential(
        &self,
        credential: engram_core::types::oauth::NewSealedOAuthCredential,
        expected_version: Option<i64>,
    ) -> Result<engram_core::types::oauth::SealedOAuthCredential, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let (version, created_at) = match db.oauth_credentials.get(&credential.key) {
            None if expected_version.is_none() => (1, now),
            Some(current) if expected_version == Some(current.version) => {
                (current.version + 1, current.created_at)
            }
            _ => return Err(MetaError::Conflict("OAuth credential CAS rejected".into())),
        };
        let row = engram_core::types::oauth::SealedOAuthCredential {
            key: credential.key.clone(),
            wrapped_dek: credential.wrapped_dek,
            nonce: credential.nonce,
            ciphertext: credential.ciphertext,
            key_id: credential.key_id,
            metadata: credential.metadata,
            version,
            created_at,
            updated_at: now,
            revoked_at: None,
            expires_at: credential.expires_at,
            broken_at: None,
            broken_reason: None,
        };
        // Publishing a validated bundle IS the repair: broken/claim state
        // clears on every successful write (PG mirrors this in the UPDATE).
        db.oauth_refresh_claims.remove(&credential.key);
        db.oauth_credentials.insert(credential.key, row.clone());
        Ok(row)
    }

    async fn list_oauth_credentials_due_for_refresh(
        &self,
        kind: engram_core::types::oauth::OAuthSubjectKind,
        now: chrono::DateTime<chrono::Utc>,
        due_before: chrono::DateTime<chrono::Utc>,
        limit: i64,
    ) -> Result<Vec<engram_core::types::oauth::SealedOAuthCredential>, MetaError> {
        self.gate()?;
        let db = self.db.lock();
        let mut due: Vec<_> = db
            .oauth_credentials
            .values()
            .filter(|row| {
                row.key.subject_kind == kind
                    && row.expires_at.is_some_and(|at| at <= due_before)
                    && row.revoked_at.is_none()
                    && row.broken_at.is_none()
                    && db
                        .oauth_refresh_claims
                        .get(&row.key)
                        .is_none_or(|until| *until < now)
            })
            .cloned()
            .collect();
        // Tie-break matches PG's ORDER BY exactly — same-second expiries must
        // pick the same subset under LIMIT on both stores (ADR 0098 D4).
        due.sort_by(|a, b| {
            (a.expires_at, &a.key.subject_id, &a.key.provider).cmp(&(
                b.expires_at,
                &b.key.subject_id,
                &b.key.provider,
            ))
        });
        due.truncate(usize::try_from(limit).unwrap_or(0));
        Ok(due)
    }

    async fn claim_oauth_refresh(
        &self,
        key: &engram_core::types::oauth::OAuthCredentialKey,
        now: chrono::DateTime<chrono::Utc>,
        until: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool, MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        let claimable = db.oauth_credentials.get(key).is_some_and(|row| {
            row.revoked_at.is_none()
                && row.broken_at.is_none()
                && db
                    .oauth_refresh_claims
                    .get(key)
                    .is_none_or(|existing| *existing < now)
        });
        if claimable {
            db.oauth_refresh_claims.insert(key.clone(), until);
        }
        Ok(claimable)
    }

    async fn mark_oauth_credential_broken(
        &self,
        key: &engram_core::types::oauth::OAuthCredentialKey,
        expected_version: i64,
        reason: &str,
    ) -> Result<engram_core::types::oauth::SealedOAuthCredential, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some(row) = db.oauth_credentials.get_mut(key) else {
            return Err(MetaError::NotFound);
        };
        if row.version != expected_version || row.revoked_at.is_some() || row.broken_at.is_some() {
            return Err(MetaError::Conflict(
                "OAuth credential version moved; reload the winner".into(),
            ));
        }
        row.broken_at = Some(now);
        row.broken_reason = Some(reason.to_owned());
        row.updated_at = now;
        let row = row.clone();
        db.oauth_refresh_claims.remove(key);
        Ok(row)
    }

    async fn revoke_oauth_credential(
        &self,
        key: &engram_core::types::oauth::OAuthCredentialKey,
        expected_version: i64,
    ) -> Result<engram_core::types::oauth::SealedOAuthCredential, MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some(row) = db.oauth_credentials.get_mut(key) else {
            return Err(MetaError::NotFound);
        };
        if row.version != expected_version || row.revoked_at.is_some() {
            return Err(MetaError::Conflict("OAuth credential CAS rejected".into()));
        }
        row.version += 1;
        row.updated_at = now;
        row.revoked_at = Some(now);
        Ok(row.clone())
    }

    async fn create_oauth_flow(
        &self,
        flow: engram_core::types::oauth::OAuthFlow,
    ) -> Result<(), MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        if db.oauth_flows.contains_key(&flow.id)
            || db.oauth_flows.values().any(|existing| {
                existing.key == flow.key
                    && existing.status == engram_core::types::oauth::OAuthFlowStatus::Pending
            })
        {
            return Err(MetaError::Conflict(
                "an OAuth flow is already pending for this subject and provider".into(),
            ));
        }
        db.oauth_flows.insert(flow.id, flow);
        Ok(())
    }

    async fn finish_oauth_flow(
        &self,
        id: uuid::Uuid,
        owner_replica: &str,
        status: engram_core::types::oauth::OAuthFlowStatus,
        error_code: Option<&str>,
    ) -> Result<(), MetaError> {
        self.gate()?;
        if !status.is_terminal() {
            return Err(MetaError::Conflict(
                "flow finish status must be terminal".into(),
            ));
        }
        let now = self.now();
        let mut db = self.db.lock();
        let Some(flow) = db.oauth_flows.get_mut(&id) else {
            return Err(MetaError::NotFound);
        };
        if flow.owner_replica != owner_replica
            || flow.status != engram_core::types::oauth::OAuthFlowStatus::Pending
            || flow.lease_expires_at <= now
        {
            return Err(MetaError::Conflict(
                "OAuth flow owner lease or status changed".into(),
            ));
        }
        flow.status = status;
        flow.error_code = error_code.map(ToOwned::to_owned);
        flow.updated_at = now;
        Ok(())
    }

    async fn get_pending_oauth_flow(
        &self,
        key: &engram_core::types::oauth::OAuthCredentialKey,
    ) -> Result<Option<engram_core::types::oauth::OAuthFlow>, MetaError> {
        self.gate()?;
        Ok(self
            .db
            .lock()
            .oauth_flows
            .values()
            .find(|flow| {
                flow.key == *key
                    && flow.status == engram_core::types::oauth::OAuthFlowStatus::Pending
            })
            .cloned())
    }

    async fn finish_oauth_flow_unowned(
        &self,
        id: uuid::Uuid,
        status: engram_core::types::oauth::OAuthFlowStatus,
        error_code: Option<&str>,
    ) -> Result<(), MetaError> {
        self.gate()?;
        if !status.is_terminal() {
            return Err(MetaError::Conflict(
                "flow finish status must be terminal".into(),
            ));
        }
        let now = self.now();
        let mut db = self.db.lock();
        let Some(flow) = db.oauth_flows.get_mut(&id) else {
            return Err(MetaError::NotFound);
        };
        if flow.status != engram_core::types::oauth::OAuthFlowStatus::Pending
            || flow.expires_at <= now
        {
            return Err(MetaError::Conflict(
                "OAuth flow is no longer pending or has expired".into(),
            ));
        }
        flow.status = status;
        flow.error_code = error_code.map(ToOwned::to_owned);
        flow.updated_at = now;
        Ok(())
    }

    async fn renew_oauth_flow_lease(
        &self,
        id: uuid::Uuid,
        owner_replica: &str,
        lease_expires_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), MetaError> {
        self.gate()?;
        let now = self.now();
        let mut db = self.db.lock();
        let Some(flow) = db.oauth_flows.get_mut(&id) else {
            return Err(MetaError::NotFound);
        };
        if flow.owner_replica != owner_replica
            || flow.status != engram_core::types::oauth::OAuthFlowStatus::Pending
            || flow.lease_expires_at <= now
            || flow.expires_at <= now
        {
            return Err(MetaError::Conflict("OAuth flow owner lease changed".into()));
        }
        flow.lease_expires_at = lease_expires_at;
        flow.updated_at = now;
        Ok(())
    }

    async fn cleanup_oauth_flows(
        &self,
        now: chrono::DateTime<chrono::Utc>,
        delete_before: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64, MetaError> {
        self.gate()?;
        let mut db = self.db.lock();
        let mut changed = 0;
        for flow in db.oauth_flows.values_mut() {
            if flow.status == engram_core::types::oauth::OAuthFlowStatus::Pending
                && (flow.expires_at <= now || flow.lease_expires_at <= now)
            {
                let expired = flow.expires_at <= now;
                flow.status = if expired {
                    engram_core::types::oauth::OAuthFlowStatus::Expired
                } else {
                    engram_core::types::oauth::OAuthFlowStatus::OwnerLost
                };
                flow.error_code = Some(
                    if expired {
                        "flow_expired"
                    } else {
                        "owner_lost"
                    }
                    .into(),
                );
                flow.updated_at = now;
                changed += 1;
            }
        }
        let before = db.oauth_flows.len();
        db.oauth_flows
            .retain(|_, flow| !flow.status.is_terminal() || flow.updated_at >= delete_before);
        Ok(changed + (before - db.oauth_flows.len()) as u64)
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
