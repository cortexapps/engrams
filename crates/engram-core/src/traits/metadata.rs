use async_trait::async_trait;

use crate::error::MetaError;
use crate::types::capability::Capability;
use crate::types::capture_job::{
    CaptureJobAssignment, CaptureJobReport, CaptureJobRow, CaptureJobStage, ColdBaseRow,
    NewCaptureJob,
};
use crate::types::event::{ArtifactRow, PersistedEvent};
use crate::types::host::{HostHeartbeat, HostRecord, HostStatus};
use crate::types::ids::{CaptureJobId, HostId, SandboxId, SessionId, SnapshotId};
use crate::types::manifest::ManifestRef;
use crate::types::registry::{
    EnableJob, EnableJobState, EnabledImage, RegistryCredential, SessionSecrets,
};
use crate::types::session::{
    DeleteHostOutcome, QueuedDemand, QueuedSession, SandboxAssignment, Session, SessionSpec,
    SessionState,
};
use crate::types::snapshot::SnapshotRecord;

/// ADR 0021 P1.8: outcome of [`MetadataStore::soft_delete_enabled_image`].
/// `Disabled` is the happy path; `AlreadyDisabled` keeps the disable
/// endpoint idempotent for retries; `Blocked` carries the offending
/// sessions so the operator can decide whether to force-stop them or
/// wait.
#[derive(Clone, Debug)]
pub enum DisableEnabledImageOutcome {
    /// Row was live; `soft_deleted_at` flipped to `NOW()` in this call.
    Disabled,
    /// Row was already soft-deleted before this call; no change made.
    /// Returned instead of an error so the disable endpoint can be
    /// safely retried (operator hit it twice, two coord pods raced).
    AlreadyDisabled,
    /// One or more sessions in `{pending, created, active, evacuating}`
    /// reference the image. `Vec` is bounded to the first 16 sessions
    /// (`ORDER BY created_at ASC LIMIT 16`) — enough for an operator to
    /// triage without unbounded response size.
    Blocked(Vec<(SessionId, String)>),
}

/// Issue #535 (b): the durable write-set for a new session — one logical
/// fact ("this session exists with these secrets/capabilities/policy/
/// harness/skills") passed to [`MetadataStore::reserve_and_persist_create`]
/// to commit in a single transaction, before any host RPC.
#[derive(Clone, Debug)]
pub struct SessionCreateWriteSet {
    pub session_id: SessionId,
    pub spec: SessionSpec,
    pub mem_budget_mib: i64,
    pub cpu_budget_vcpus: i32,
    /// KEK-sealed BEFORE this call — async crypto has no place inside a DB
    /// transaction. `None` when there are no per-request secret overrides.
    pub sealed_secrets: Option<SessionSecrets>,
    /// ADR 0056: profile-granted capabilities, already parsed + validated.
    pub capabilities: Vec<Capability>,
    /// ADR 0056 (B′): the compiled integration policy, pre-serialized to
    /// JSON (mirrors `bind_session_integration_policy`'s wire shape).
    pub integration_policy_json: Option<String>,
    /// ADR 0077 phase 3: the session's boot inputs as ONE persisted
    /// document — `selected_skills` (the TODO(P1-D) fix), `selected_harness`
    /// (ADR 0062 catalog key), and `workdir`. Written into the
    /// `session_runtime_specs` row in the SAME create transaction, and
    /// consumed by queue re-prepare instead of re-derived. This is the
    /// single source that subsumes #566's interim
    /// `sessions.selected_skills` column (retired, migration 0090).
    /// `reserve_and_persist_create` also mirrors `runtime_spec
    /// .selected_harness` into the pre-existing `sessions.harness` column.
    pub runtime_spec: crate::types::runtime_spec::RuntimeSpec,
}

/// One candidate host's fit verdict from
/// [`MetadataStore::placement_no_fit_details`]. `reason` is a bounded
/// vocabulary (metric-label safe): `ram_full` / `cpu_full` /
/// `unmeasured` (no allocatable measurement yet) / `not_lockable`
/// (status/cordon changed between ranking and the pick) / `fits_now`
/// (freed up since the failed pick — indicates a race, not a bug).
#[derive(Clone, Debug)]
pub struct PlacementNoFit {
    pub host_id: HostId,
    pub reason: &'static str,
    /// `allocatable − reserved` at read time (MiB; meaningless when
    /// `reason == "unmeasured"`).
    pub free_mib: i64,
    /// `cpu_budget − reserved_vcpus` at read time (0-gated hosts report
    /// `i64::MAX` — CPU is ungated there).
    pub free_vcpus: i64,
}

/// Outcome of [`MetadataStore::reserve_and_persist_create`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateDisposition {
    /// A candidate host fit both budgets; the row is `pending` on this
    /// host, ready for `boot_on_reserved_host`.
    Placed(HostId),
    /// No candidate fit; the row is `queued` for the scanner, carrying the
    /// identical satellites its later re-prepare will find.
    Queued,
}

/// ADR 0073 phase 4: one idle-scan candidate row (Active + bound).
#[derive(Clone, Debug)]
pub struct IdleScanCandidate {
    pub session_id: SessionId,
    pub sandbox_id: Option<crate::SandboxId>,
    pub host_id: Option<crate::HostId>,
    /// Newest session_event time, falling back to the session's
    /// created_at when no events exist yet.
    pub last_event_at: chrono::DateTime<chrono::Utc>,
    /// Kind of the newest event (`None` = no events yet).
    pub last_event_kind: Option<String>,
    pub shell_pinned_until: Option<chrono::DateTime<chrono::Utc>>,
}

/// Authoritative source of truth. Postgres-backed in v1; trait exists so
/// we can support SQLite for embedded deployments later.
#[async_trait]
pub trait MetadataStore: Send + Sync {
    // ---- liveness ----
    //
    // Cheap connectivity check for readiness probes. Default is
    // `Ok(())` so in-memory test stores don't need to override.
    // Postgres-backed impls should issue a `SELECT 1` against the
    // pool so a coordinator with a broken DB connection fails its
    // `/readyz` probe instead of receiving traffic that 503s.
    async fn ping(&self) -> Result<(), MetaError> {
        Ok(())
    }

    // ---- sessions ----
    async fn create_session(&self, spec: SessionSpec) -> Result<SessionId, MetaError>;

    /// Atomically insert a session in `Created` status with `host_id`
    /// and `sandbox_id` already bound. Used by the create-session API
    /// to only persist a row once scheduling has succeeded — so a
    /// transient capacity blip or unrecoverable scheduling error
    /// doesn't leave a `Pending` row that nothing will ever advance.
    ///
    /// ADR 0015 M2: the row enters life at `Created` (sandbox bound,
    /// nothing else proven) rather than `Active` (the prior shape).
    /// The create handler transitions to `Active` only after
    /// `start_agent` succeeds, so `Active` actually implies "agentd
    /// reachable + harness running" — no more silent dishonest-Active
    /// rows where start_agent later failed but the column already
    /// said Active.
    ///
    /// The caller mints the `SessionId` ahead of scheduling (because
    /// vm_spec env / harness arg construction needs it before the
    /// sandbox exists). The impl persists with that exact id.
    ///
    /// TODO(self-healing reconciler): a future version of this API
    /// reintroduces a "queue-and-retry" path — pending sessions
    /// persist with the full vm_spec captured, and a background
    /// reconciler retries scheduling against later-arriving capacity.
    /// At that point this method might fall back to "insert Pending
    /// if scheduling fails, the reconciler picks it up later". The
    /// reason we don't do that today: SessionSpec doesn't carry
    /// vm_spec / harness resolution context, and capturing it
    /// requires schema work that's bigger than the v1 fix.
    ///
    /// Issue #535 (c): flip an already-`pending` row (committed by
    /// [`MetadataStore::reserve_and_persist_create`] before any host RPC
    /// ran) to `Created`, binding `sandbox_id`. Replaces the old
    /// `create_session_created`'s INSERT-or-UPDATE upsert — the row is now
    /// GUARANTEED to already exist, so this is a single `UPDATE`. The
    /// UPDATE is still guarded on `status = 'pending'`: a delete or a
    /// stale-pending requeue can race the in-flight restore RPC that
    /// precedes this call, so "exists" is not "still pending". Returns
    /// [`MetaError::NotFound`] if the row is gone or no longer `pending`,
    /// so the caller's teardown arm runs instead of silently binding
    /// `sandbox_id` onto an inconsistent row.
    async fn transition_session_created(
        &self,
        session_id: SessionId,
        sandbox_id: SandboxId,
    ) -> Result<(), MetaError>;

    async fn get_session(&self, id: SessionId) -> Result<Session, MetaError>;

    /// ADR 0090: reverse ownership lookup — which session (if any)
    /// currently binds `sandbox_id` on `host_id`? Answers the teardown
    /// reconciler's unknown-binding arm: a host-agent generation whose
    /// in-memory binding table missed a sandbox (NBD rehydrate failed
    /// mid-roll) must ask the coordinator before counting an orphan
    /// strike — local absence-of-binding is NOT ownership truth
    /// (2026-07-11 campaign: a healthy pidfd-reattached VM was SIGKILLed
    /// mid-build on exactly this). Non-terminal sessions only (`failed`/
    /// `completed`/`dead` don't own anything; `host_lost` DOES — its
    /// surviving VM is what recovery is for). Default impl (mocks): none.
    async fn session_owning_sandbox(
        &self,
        _host_id: HostId,
        _sandbox_id: crate::SandboxId,
    ) -> Result<Option<SessionId>, MetaError> {
        Ok(None)
    }

    /// Every live (non-terminal, non-`host_lost`) session: `pending`,
    /// `created`, `active`, `idle`, `evacuating`, `evicting`. This is
    /// the rehydration source for the coord's in-memory routing maps
    /// (`repopulate_routing`) — every state
    /// that can carry a live `sandbox_id` binding (`evicting`
    /// included: the sandbox stays bound while the eviction pipeline
    /// runs) MUST be listed here, or a coord restart strands the
    /// session with an unroutable sandbox.
    async fn list_active_sessions(&self) -> Result<Vec<Session>, MetaError>;

    /// Diagnostic twin of the `reserve_and_persist_create` /
    /// `place_queued_session` 2D pick: for each candidate host, why the
    /// session's `(mem_budget_mib, cpu_budget_vcpus)` does not fit right
    /// now. Read OUTSIDE the placement transaction (no `FOR UPDATE`) —
    /// a snapshot for observability, so a host that frees up between the
    /// failed pick and this read can honestly report `fits_now`. Called
    /// only on the no-capacity path (2026-07-11 campaign: every host was
    /// rejected with zero per-host visibility — the "no capacity with
    /// free hosts" mystery). Default impl (mock stores): empty.
    async fn placement_no_fit_details(
        &self,
        _candidates: &[HostId],
        _mem_budget_mib: i64,
        _cpu_budget_vcpus: i32,
    ) -> Result<Vec<PlacementNoFit>, MetaError> {
        Ok(Vec::new())
    }

    /// Issue #535 (b): the ENTIRE session write-set — one logical fact
    /// ("this session exists with these secrets/capabilities/policy/
    /// harness/skills") — committed in ONE transaction, before any host RPC.
    /// Subsumes the old `reserve_placement` (ADR 0046/0048 atomic pick-a-
    /// host-from-`candidates` + reserve both `mem_budget_mib` and
    /// `cpu_budget_vcpus`, RAM: `allocatable − reserved`, CPU: `total_vcpus
    /// × overcommit − reserved`) AND `enqueue_session_create` (the no-
    /// capacity fallback) AND the satellite writes both the boot path and
    /// the enqueue path used to make SEPARATELY, after the row already
    /// existed, each individually warn-and-continue: `persist_session_
    /// secrets`, `bind_session_capabilities`, `bind_session_integration_
    /// policy`, `set_session_harness`.
    ///
    /// Returns [`CreateDisposition::Placed`] (a candidate fit — the row is
    /// `pending` on that host, ready for `boot_on_reserved_host`) or
    /// [`CreateDisposition::Queued`] (none fit — the row is `queued` for the
    /// scanner, carrying the identical satellites so its later re-prepare
    /// finds them). The FK-ordering bug class (minting a broker token or
    /// binding a capability before the row exists silently no-ops — the
    /// ADR 0051 forge-token regression) is dead by construction: nothing
    /// downstream of this call can observe a partially-written session.
    ///
    /// The Postgres impl extends `reserve_placement`'s `FOR UPDATE`
    /// transaction (concurrent placers, any replica, serialize on the
    /// candidate host rows) to also insert the satellite rows in the SAME
    /// transaction. The KEK seal (async crypto) and any external credential
    /// mint are NOT this call's concern — they happen before
    /// (`ws.sealed_secrets` arrives pre-sealed) or after (broker-token mint,
    /// deferred to the boot pipeline where the FK is already satisfiable).
    ///
    /// No default: unlike the old `reserve_placement`'s "just pick a
    /// candidate, don't reserve anything" fallback, this call's job now
    /// includes the row insert — there's no harmless no-op shape for that
    /// (mirrors why the old `create_session_created` it partly replaces was
    /// also required). Every `MetadataStore` impl must decide honestly: a
    /// mock that never exercises the create path can `unreachable!()`, like
    /// it already does for other unexercised trait surface; one that does
    /// (the coordinator's HTTP/gRPC integration-test mocks) implements the
    /// real in-memory equivalent.
    async fn reserve_and_persist_create(
        &self,
        ws: SessionCreateWriteSet,
        candidates: &[HostId],
        affinity_len: usize,
    ) -> Result<CreateDisposition, MetaError>;

    /// ADR 0046: release a reservation whose boot failed, by deleting its
    /// `pending`, sandbox-less row. Default impl (mocks) is a no-op.
    async fn delete_pending_session(&self, _session_id: SessionId) -> Result<(), MetaError> {
        Ok(())
    }

    // ---- ADR 0048: session queue ----

    /// Park an `Idle` session that hit no capacity on resume back in the
    /// queue (`Idle → queued`, `queue_origin = 'resume'`), fenced by the
    /// resume op's epoch (ADR 0079: `sessions.current_epoch = epoch` —
    /// every sibling write in the op pipeline is fenced, and this one
    /// must be too or a reclaimed-away zombie executor forks the state
    /// machine). Returns whether the flip landed: `false` means the row
    /// was no longer `Idle` (a racing writer advanced it) OR the epoch
    /// moved (a successor re-claimed) — either way the caller must stop
    /// without emitting the Queued event. Default no-op: `false`.
    async fn enqueue_session_resume(&self, _id: SessionId, _epoch: i64) -> Result<bool, MetaError> {
        Ok(false)
    }

    /// Every `queued` session, oldest-first (FIFO). The scanner walks
    /// this each tick. Default impl (mocks): empty.
    async fn list_queued_sessions_fifo(&self) -> Result<Vec<QueuedSession>, MetaError> {
        Ok(Vec::new())
    }

    /// Atomically re-attempt placement for a `queued` session: pick a
    /// host from `candidates` (same best-fit 2D logic as
    /// `reserve_and_persist_create`'s placement leg) and, if one fits, flip the row
    /// `queued → pending` with the host bound + `last_active_at` bumped,
    /// returning the host. `None` = nothing fit (stay queued) or the row
    /// already left `queued` (lost a race). Default impl (mocks): place
    /// on the first candidate.
    async fn place_queued_session(
        &self,
        _id: SessionId,
        _mem_budget_mib: i64,
        _cpu_budget_vcpus: i32,
        candidates: &[HostId],
        _affinity_len: usize,
    ) -> Result<Option<HostId>, MetaError> {
        Ok(candidates.first().copied())
    }

    // ADR 0079 (issue #543): `requeue_session` + `requeue_stale_pending`
    // are retired — a placed queued session's boot rides the create_boot
    // op (`session_ops`), whose row-level `not_before`/`attempts` own the
    // retry and whose reclaim sweep owns the coord-died-mid-boot recovery.

    /// The queue's demand (count, Σ mem_budget_mib, Σ cpu_budget_vcpus) —
    /// the scale-up signal the operator reads via `/admin/fleet/demand`.
    /// Default: zeros.
    async fn queued_demand(&self) -> Result<QueuedDemand, MetaError> {
        Ok(QueuedDemand::default())
    }

    /// ADR 0047 (was `state.teleport_targets`): pin / clear the
    /// operator-chosen teleport destination on the session row. The
    /// evac scanner — on ANY replica — honors the pin as its required
    /// placement. Setting a pin stamps `teleport_target_set_at = NOW()`
    /// (issue #214) so the scanner can age out a stale leaked pin;
    /// clearing (`None`) clears the stamp too. Default impls (mocks):
    /// no-op / no pin.
    async fn set_teleport_target(
        &self,
        _id: SessionId,
        _target: Option<HostId>,
    ) -> Result<(), MetaError> {
        Ok(())
    }
    /// Read the operator-pinned teleport destination and the instant it
    /// was set, if any. Issue #214: the `set_at` lets the evac scanner
    /// ignore + clear a pin older than a TTL — degrading any future pin
    /// leak to default placement instead of a strict hijack. A `None`
    /// timestamp (pin set before the 0065 migration) is treated as
    /// not-aged by the scanner. Default impls (mocks): no pin.
    async fn get_teleport_target(
        &self,
        _id: SessionId,
    ) -> Result<Option<(HostId, Option<chrono::DateTime<chrono::Utc>>)>, MetaError> {
        Ok(None)
    }

    /// ADR 0047 (was `state.git_broker_tokens`): the KEK-sealed
    /// per-session broker token. `insert_broker_token` is
    /// first-writer-wins (`ON CONFLICT DO NOTHING`) and returns whether
    /// THIS call inserted — a `false` means a sibling replica won the
    /// mint race and the caller re-reads. Default impls (mocks):
    /// insert always "wins", get finds nothing, delete no-ops — mock
    /// flows ride the in-memory cache alone, which is exactly the
    /// pre-0047 behavior.
    async fn insert_broker_token(
        &self,
        _token: crate::types::registry::SessionBrokerToken,
    ) -> Result<bool, MetaError> {
        Ok(true)
    }
    async fn get_broker_token(
        &self,
        _id: SessionId,
    ) -> Result<Option<crate::types::registry::SessionBrokerToken>, MetaError> {
        Ok(None)
    }
    async fn delete_broker_token(&self, _id: SessionId) -> Result<(), MetaError> {
        Ok(())
    }

    /// ADR 0057: the KEK-sealed, admin-managed org secret store. The org
    /// `SecretStore` backend resolves these through the composed store; the
    /// admin `OrgSecretService` writes them (sealed at the coordinator).
    /// `upsert` is name-keyed (`ON CONFLICT (name) DO UPDATE`) and fires
    /// `pg_notify('org_secret_changed', name)` so the mint broker invalidates
    /// its cache (ADR 0057 C2). `list` returns metadata only — NEVER ciphertext.
    /// Default impls (mocks): upsert/delete no-op, get/list find nothing.
    async fn upsert_org_secret(
        &self,
        sealed: crate::types::org_secret::SealedOrgSecret,
    ) -> Result<crate::types::org_secret::OrgSecret, MetaError> {
        Ok(crate::types::org_secret::OrgSecret {
            name: sealed.name,
            key_id: sealed.key_id,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
    }
    async fn get_org_secret_sealed(
        &self,
        _name: &str,
    ) -> Result<Option<crate::types::org_secret::SealedOrgSecret>, MetaError> {
        Ok(None)
    }
    async fn list_org_secrets(
        &self,
    ) -> Result<Vec<crate::types::org_secret::OrgSecret>, MetaError> {
        Ok(Vec::new())
    }
    async fn delete_org_secret(&self, _name: &str) -> Result<bool, MetaError> {
        Ok(false)
    }

    /// ADR 0047 (replica-safe reconciler): apply one heartbeat's
    /// missing-sandbox strike accounting on the session rows. Sessions
    /// whose sandbox WAS in the heartbeat get their counter reset;
    /// missing ones increment; ids that reach `grace_ticks` are
    /// returned (their counters reset in the same transaction) and the
    /// caller flips them. The shared column restores the "missing N
    /// CONSECUTIVE heartbeats" semantics that per-pod counters corrupt
    /// when a host's heartbeats round-robin across replicas. Default
    /// impl (mocks): never flips; reconcile-exercising mocks override.
    async fn apply_missing_sandbox_strikes(
        &self,
        _present: &[SessionId],
        _missing: &[SessionId],
        _grace_ticks: i32,
    ) -> Result<Vec<SessionId>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0047/0048: per-host reserved budget (Σ `mem_budget_mib` AND
    /// Σ `cpu_budget_vcpus` over the memory-reserving session states) —
    /// the read-side twin of `reserve_and_persist_create`'s aggregate, for the
    /// capacity-soft resume/evac picker and the fleet view. Default impl
    /// (mocks): empty map (no reservations).
    async fn per_host_reserved(
        &self,
    ) -> Result<std::collections::HashMap<HostId, crate::types::host::ReservedBudget>, MetaError>
    {
        Ok(std::collections::HashMap::new())
    }

    /// ADR 0009 reconcile pass: enumerate the `(session_id,
    /// sandbox_id)` pairs for every `status='active'` session
    /// assigned to `host_id` whose `sandbox_id` is populated. The
    /// reconcile pass intersects this against the host's
    /// heartbeat-reported `running_sandboxes`. Missing-from-host
    /// → strike counter increments; N strikes → flip per ADR §3.
    ///
    /// Default impl scans `list_active_sessions()` and filters in
    /// memory — fine for in-memory test mocks. Postgres overrides
    /// with an indexed `WHERE (host_id, status)` query so the
    /// per-heartbeat cost stays O(sandboxes-on-host), not
    /// O(total-active-sessions).
    async fn list_active_sandbox_assignments_on_host(
        &self,
        host_id: HostId,
    ) -> Result<Vec<(SessionId, SandboxId)>, MetaError> {
        let all = self.list_active_sessions().await?;
        Ok(all
            .into_iter()
            .filter_map(|s| match (s.status, s.host_id, s.sandbox_id) {
                (SessionState::Active, Some(h), Some(sb)) if h == host_id => Some((s.id, sb)),
                _ => None,
            })
            .collect())
    }

    /// ADR 0048 C8: the Active assignments on `host_id` WITH their
    /// reservation budgets, for the drain don't-strand guard (it must
    /// pre-check that some survivor fits each session's budgets before
    /// starting a move). Default impl scans `list_active_sessions` (mocks
    /// carry no budgets → 0, which the guard treats as "no constraint").
    async fn list_active_assignments_with_budgets_on_host(
        &self,
        host_id: HostId,
    ) -> Result<Vec<SandboxAssignment>, MetaError> {
        let all = self.list_active_sessions().await?;
        Ok(all
            .into_iter()
            .filter_map(|s| match (s.status, s.host_id, s.sandbox_id) {
                (SessionState::Active, Some(h), Some(sb)) if h == host_id => {
                    Some(SandboxAssignment {
                        session_id: s.id,
                        sandbox_id: sb,
                        mem_budget_mib: 0,
                        cpu_budget_vcpus: 0,
                    })
                }
                _ => None,
            })
            .collect())
    }

    /// ADR 0048: deregister a drained host immediately — its `hosts` row
    /// is deleted so the operator's scale-down doesn't wait ~30-40s for
    /// the dead-host detector. REFUSES (returns the bound count) if any
    /// session is still bound (`pending`/`created`/`active`/`evacuating`/
    /// `evicting`); idempotent (a missing row = `Ok(Deleted)`).
    /// Default impl (mocks): `Deleted`.
    async fn delete_host(&self, _id: HostId) -> Result<DeleteHostOutcome, MetaError> {
        Ok(DeleteHostOutcome::Deleted)
    }

    /// ADR 0016 Phase B commit 7 — restart-time rehydration source.
    /// Returns one record per Active session bound to a sandbox on
    /// `host_id`, including the effective disk manifest the host
    /// should rebuild `ChunkedDiskBackend` from: the newer of
    /// `sessions.live_disk_manifest_*` (last FlushScheduler publish)
    /// and the latest recoverable snapshot's `disk_manifest`.
    /// Same resolver semantic as `effective_resume_disk_manifest`
    /// (commit 6); kept server-side so the host issues one query
    /// per restart instead of N+1 round-trips.
    ///
    /// Default impl: scans `list_active_sessions()` + walks
    /// `list_session_snapshots(...)` per row. Postgres overrides
    /// with a single LEFT JOIN. The default exists so test mocks
    /// without a snapshot index still satisfy the trait.
    ///
    /// `Option<ManifestRef>` is `None` when:
    /// - The session never published a live manifest AND has no
    ///   recoverable snapshot — legacy session or sandbox without
    ///   chunked-disk tracking. Host skips rehydration (no
    ///   `attach_chunked_disk` to call).
    /// - The session's status changed mid-query (defensive).
    async fn list_active_sandboxes_on_host_with_disk_manifest(
        &self,
        host_id: HostId,
    ) -> Result<Vec<(SessionId, SandboxId, Option<ManifestRef>)>, MetaError> {
        let all = self.list_active_sessions().await?;
        let mut out = Vec::new();
        for s in all {
            if !matches!(s.status, SessionState::Active) {
                continue;
            }
            let (Some(h), Some(sb)) = (s.host_id, s.sandbox_id) else {
                continue;
            };
            if h != host_id {
                continue;
            }
            // Pick newer of live vs. latest snapshot. The default
            // impl can't cheaply query "latest snapshot"; default
            // to live_disk_manifest only and let PgMeta's override
            // do the JOIN.
            out.push((s.id, sb, s.live_disk_manifest));
        }
        Ok(out)
    }
    /// ADR 0015 M2: the single validated entry point for `UPDATE
    /// sessions SET status = ...`. Reads the current state, runs
    /// [`SessionState::try_transition_to`] against `target`, and
    /// writes the UPDATE atomically. Returns the previous state on
    /// success — callers use it as the `from` of the `StatusChanged`
    /// event they emit.
    ///
    /// Errors:
    /// - [`MetaError::NotFound`] — no row with this `id`.
    /// - [`MetaError::Conflict`] — the transition is not in the
    ///   legality table. The message is the rendered
    ///   [`crate::types::session::IllegalTransition`] so logs and HTTP
    ///   bodies show both sides.
    ///
    /// Impls must do the SELECT and UPDATE under a single row-level
    /// lock to prevent two concurrent callers racing on the same
    /// (from, to) — without that, both could validate against the
    /// same pre-state and the second's UPDATE silently breaks the
    /// invariant.
    async fn transition_session(
        &self,
        id: SessionId,
        target: SessionState,
    ) -> Result<SessionState, MetaError>;

    /// Force a session to its FSM-legal terminal state — the shared
    /// "delete / give up on this session" primitive (the delete handler
    /// uses it; drain / dead-host paths can too). Reads the current state,
    /// picks the terminal [`SessionState::terminal_target`] permits
    /// (`Completed` for states that ran, `Failed` for ones that never
    /// became usable), and drives [`Self::transition_session`] to it.
    /// Returns `Some((prev, target))` on a transition, `None` if the
    /// session is already terminal (idempotent no-op).
    ///
    /// The default impl composes `get_session` + `transition_session`, so
    /// it reuses the latter's row-locked atomic write rather than
    /// duplicating the UPDATE; only the target choice is read separately,
    /// and a race there is self-correcting — the transition is still legal
    /// for the new state, or returns `Conflict` for a now-terminal row.
    async fn terminate_session(
        &self,
        id: SessionId,
    ) -> Result<Option<(SessionState, SessionState)>, MetaError> {
        let session = self.get_session(id).await?;
        let Some(target) = session.status.terminal_target() else {
            return Ok(None);
        };
        let prev = self.transition_session(id, target).await?;
        Ok(Some((prev, target)))
    }

    async fn assign_session_host(
        &self,
        id: SessionId,
        host_id: Option<HostId>,
    ) -> Result<(), MetaError>;

    /// Persist the in-memory `SandboxId` of the live sandbox serving
    /// this session. Set to `Some` after `host_registry.create_for_session`
    /// returns, cleared to `None` on evict/migrate. The coordinator
    /// uses these rows to rebuild its in-memory routing maps after
    /// a restart.
    async fn assign_session_sandbox(
        &self,
        id: SessionId,
        sandbox_id: Option<SandboxId>,
    ) -> Result<(), MetaError>;

    /// ADR 0073: mint the next binding epoch for `id` — one atomic
    /// `UPDATE … SET binding_epoch = binding_epoch + 1 … RETURNING`.
    /// Called by the coordinator at the moment it commits to binding
    /// the session to a NEW sandbox for a fresh-spawn flow (create,
    /// idle resume, cold recovery, evac). Live moves do NOT mint — the
    /// harness process survives a teleport and its generation is
    /// unchanged (see `current_binding_epoch`).
    ///
    /// Default (mock stores): a constant `1` — mocks get "no fencing",
    /// which is the pre-0067 behavior; the Postgres store overrides
    /// with the real per-session counter. Same degradation pattern as
    /// the guarded-CAS defaults above.
    async fn mint_binding_epoch(&self, id: SessionId) -> Result<u64, MetaError> {
        let _ = id;
        Ok(1)
    }

    /// ADR 0073: the session's current binding epoch, without minting.
    /// Read by flows where the harness process may SURVIVE the
    /// transition (live migration; in-place reattach on the same
    /// sandbox) so the spec they build matches the standing record.
    ///
    /// Default (mock stores): constant `1`, paired with
    /// [`Self::mint_binding_epoch`]'s default.
    async fn current_binding_epoch(&self, id: SessionId) -> Result<u64, MetaError> {
        let _ = id;
        Ok(1)
    }

    /// ADR 0073 phase 4: one row per Active+bound session for the idle
    /// scan — newest event (kind + time), host, and the shell pin. The
    /// detector classifies soft/hard client-side so the TTL policy
    /// lives in one place.
    async fn list_idle_scan_candidates(
        &self,
        soft_ttl_secs: i64,
        hard_ttl_secs: i64,
    ) -> Result<Vec<IdleScanCandidate>, MetaError> {
        let _ = (soft_ttl_secs, hard_ttl_secs);
        Ok(Vec::new())
    }

    /// ADR 0074: stamp the parking-ladder rung (and its entry time;
    /// `None` clears both). Bookkeeping only — the FSM `status` stays
    /// authoritative for lifecycle legality.
    async fn set_session_park_rung(
        &self,
        id: SessionId,
        rung: i16,
        parked_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<(), MetaError> {
        let _ = (id, rung, parked_at);
        Ok(())
    }

    /// Session titles: record the latest harness-suggested (LLM-generated)
    /// title on the session row. Idempotent; called from the harness event
    /// sink the moment a `TitleSuggested` event arrives, so the orchestrator
    /// can surface it as the session's display title without walking the log.
    /// A self-descriptive operational fact — never user attribution.
    async fn set_session_suggested_title(
        &self,
        id: SessionId,
        title: &str,
    ) -> Result<(), MetaError> {
        let _ = (id, title);
        Ok(())
    }

    /// ADR 0079 (review finding #6): the FENCED park-rung write for
    /// op-path callers (rung-2 park bookkeeping + its compensations, the
    /// idle-evict park clear). Appends `AND current_epoch = $e`; `Ok(false)`
    /// ⇒ a successor re-claimed — the stamp is a no-op so a fenced-out
    /// predecessor's `set_session_park_rung(0)` compensation can never wipe
    /// the successor's fresh `park_rung = 2` (the #585 stall reborn). The
    /// out-of-op nomination (idle_detector rung 1) keeps the unfenced
    /// variant above.
    async fn fenced_set_session_park_rung(
        &self,
        id: SessionId,
        epoch: i64,
        rung: i16,
        parked_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<bool, MetaError> {
        let _ = (id, epoch, rung, parked_at);
        Err(MetaError::Serialization(
            "fenced writes not supported by this store".into(),
        ))
    }

    /// ADR 0073 phase 4: stamp/renew the shell keep-alive pin. The WS
    /// bridge calls this on its keepalive; passing a past instant (or
    /// letting it lapse) un-pins.
    async fn stamp_shell_pin(
        &self,
        id: SessionId,
        pinned_until: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), MetaError> {
        let _ = (id, pinned_until);
        Ok(())
    }

    // ----------------------------------------------------------------
    // ADR 0079 (issue #543) — the durable per-session op log. Every
    // lifecycle verb is a `session_ops` row; `sessions.current_epoch`
    // is the fencing epoch, CAS-bumped in the SAME transaction that
    // claims an op and appended (`AND current_epoch = $e`) to every
    // session-row write an op makes. Defaults are mock-benign (no-op /
    // empty); PostgresStore overrides with the real SQL. Tests that
    // assert op semantics use the live-PG harness.
    // ----------------------------------------------------------------

    /// Enqueue a lifecycle op and, when NOTHING is running or queued
    /// ahead of it for this session, claim it in the same transaction
    /// (CAS-bumping `current_epoch`). The idle-session happy path is one
    /// PG round trip. Fires `pg_notify('session_ops', session_id)`.
    async fn op_enqueue_and_claim(
        &self,
        session_id: SessionId,
        kind: crate::types::session_op::OpKind,
        payload: serde_json::Value,
        idempotency_key: Option<&str>,
        claimed_by: &str,
    ) -> Result<crate::types::session_op::EnqueueOutcome, MetaError> {
        let _ = (session_id, kind, payload, idempotency_key, claimed_by);
        Err(MetaError::Serialization(
            "op log not supported by this store".into(),
        ))
    }

    /// ADR 0079 (review finding #8): an ATOMIC claim-or-fail for inline
    /// claims (`OpClaim` — admin evacuate/drain, evac-resumer, teleport).
    /// Inserts AND claims the op in one transaction IFF the session's op
    /// lane is free (nothing running, nothing else queued); otherwise
    /// ROLLS BACK so no grabbable `queued` row is ever left behind (the
    /// enqueue-then-`op_cancel_by_id` shape had a window where the
    /// executor could claim the row between INSERT-commit and the cancel,
    /// running a full verb the caller was told it couldn't). `Ok(None)` =
    /// the lane is busy — the caller reports "busy" and the row does not
    /// exist.
    async fn op_enqueue_and_claim_exclusive(
        &self,
        session_id: SessionId,
        kind: crate::types::session_op::OpKind,
        payload: serde_json::Value,
        claimed_by: &str,
    ) -> Result<Option<crate::types::session_op::SessionOp>, MetaError> {
        let _ = (session_id, kind, payload, claimed_by);
        Err(MetaError::Serialization(
            "op log not supported by this store".into(),
        ))
    }

    /// Claim the head queued op for `session_id` (if due and nothing is
    /// running): CAS-bump `current_epoch`, stamp the row `running` with
    /// the new epoch. `Ok(None)` = nothing claimable.
    async fn op_claim_head(
        &self,
        session_id: SessionId,
        claimed_by: &str,
    ) -> Result<Option<crate::types::session_op::SessionOp>, MetaError> {
        let _ = (session_id, claimed_by);
        Ok(None)
    }

    /// Sessions with at least one due queued op — the executor's scan
    /// input (NOTIFY is the hot path; this backs the fallback poll and
    /// startup recovery).
    async fn op_due_sessions(&self) -> Result<Vec<SessionId>, MetaError> {
        Ok(Vec::new())
    }

    /// Record the op's durable step marker + progress heartbeat. Fenced:
    /// the UPDATE carries `AND epoch = $e AND state = 'running'`; 0 rows
    /// ⇒ a successor re-claimed (the caller must STOP silently).
    async fn op_record_step(&self, op_id: i64, epoch: i64, step: &str) -> Result<bool, MetaError> {
        let _ = (op_id, epoch, step);
        Ok(true)
    }

    /// ADR 0079 (review finding #1): bump ONLY `heartbeat_at` (not
    /// `step`), so a within-step background heartbeat can prove liveness
    /// while a step's body is in flight across a multi-second-to-minute
    /// host RPC (restore's ~92s GCS page-in, composed capture/upload)
    /// WITHOUT clobbering the crash-resume step marker. Fenced like
    /// `op_record_step`; `Ok(false)` ⇒ a successor re-claimed (stop the
    /// heartbeat loop).
    async fn op_heartbeat(&self, op_id: i64, epoch: i64) -> Result<bool, MetaError> {
        let _ = (op_id, epoch);
        Ok(true)
    }

    /// Finish an op: `done` (success), or terminal `failed` with error
    /// text. Fenced like `op_record_step`. Returns whether the row was
    /// ours to finish.
    async fn op_finish(
        &self,
        op_id: i64,
        epoch: i64,
        state: crate::types::session_op::OpState,
        error: Option<&str>,
    ) -> Result<bool, MetaError> {
        let _ = (op_id, epoch, state, error);
        Ok(true)
    }

    /// Retryable failure: back to `queued` with `not_before = now() +
    /// backoff` (attempts already counted at claim). Fenced.
    async fn op_requeue_with_backoff(
        &self,
        op_id: i64,
        epoch: i64,
        backoff: std::time::Duration,
        error: &str,
    ) -> Result<bool, MetaError> {
        let _ = (op_id, epoch, backoff, error);
        Ok(true)
    }

    /// Wake every QUEUED op of `kind` for this session by resetting its
    /// `not_before` to now (and NOTIFY). ADR 0079 latency fix: the deliver
    /// verb, on an Idle session, enqueues a Resume op and requeues itself
    /// with a FAILURE backoff — but the resume completing is not a
    /// failure, and the backed-off deliver would otherwise wait out the
    /// 5 s fallback poll after the resume finishes (prod: prompt-after-
    /// idle regressed ~10 s → ~21 s). When a `for_delivery` resume reaches
    /// terminal, we wake its sibling deliver so the executor's completion
    /// re-drive claims it in <100 ms. A no-spin wake (only fired on the
    /// resume's terminal, never while it runs — the one-running slot
    /// already blocks the deliver during the resume). Returns rows woken.
    async fn op_wake_queued_kind(
        &self,
        session_id: SessionId,
        kind: crate::types::session_op::OpKind,
    ) -> Result<u64, MetaError> {
        let _ = (session_id, kind);
        Ok(0)
    }

    /// Cancel a still-queued op (`queued → cancelled`). Running ops are
    /// cancelled cooperatively via [`Self::op_cancel_requested`].
    async fn op_cancel_queued(
        &self,
        session_id: SessionId,
        kind: crate::types::session_op::OpKind,
    ) -> Result<bool, MetaError> {
        let _ = (session_id, kind);
        Ok(false)
    }

    /// Set the cancel flag on the RUNNING op of `kind` (payload-embedded
    /// `_cancel: true`); the executor checks it between steps.
    async fn op_request_cancel_running(
        &self,
        session_id: SessionId,
        kind: crate::types::session_op::OpKind,
    ) -> Result<bool, MetaError> {
        let _ = (session_id, kind);
        Ok(false)
    }

    /// The RUNNING op's cancel flag (executor-side check between steps).
    async fn op_cancel_requested(&self, op_id: i64) -> Result<bool, MetaError> {
        let _ = op_id;
        Ok(false)
    }

    /// The session's currently-running op, if any — the "is a resume in
    /// flight" visibility read (no `Resuming` FSM state; the op row IS
    /// the visibility).
    async fn op_running_for(
        &self,
        session_id: SessionId,
    ) -> Result<Option<crate::types::session_op::SessionOp>, MetaError> {
        let _ = session_id;
        Ok(None)
    }

    /// One op row by id — the bounded-observe read (wire handlers poll a
    /// just-enqueued op to relay its terminal outcome to the caller).
    async fn op_get(
        &self,
        op_id: i64,
    ) -> Result<Option<crate::types::session_op::SessionOp>, MetaError> {
        let _ = op_id;
        Ok(None)
    }

    /// Cancel ONE still-queued op by id. The inline-claim helper uses
    /// this to withdraw its own row when the enqueue landed behind an
    /// in-flight op (claim-or-give-up semantics) without collaterally
    /// cancelling other queued ops of the same kind.
    async fn op_cancel_by_id(&self, op_id: i64) -> Result<bool, MetaError> {
        let _ = op_id;
        Ok(false)
    }

    /// Is a queued-or-running op of `kind` already pending for this
    /// session? The cheap duplicate guard for wake-driven enqueuers (the
    /// outbox delivery shim fires per due session per wake; without this
    /// every wake would append another identical Deliver row). Advisory —
    /// a racing enqueue may still slip a duplicate through, which is
    /// harmless (the duplicate finds no due work and completes).
    async fn op_pending_exists(
        &self,
        session_id: SessionId,
        kind: crate::types::session_op::OpKind,
    ) -> Result<bool, MetaError> {
        let _ = (session_id, kind);
        Ok(false)
    }

    /// ADR 0079 (review finding #5): PENDING sessions that lost their
    /// create_boot op — placed (`queued → pending`, host reserved) but no
    /// active (`queued|running`) create_boot op exists, older than
    /// `older_than` (so a just-placed session whose op enqueue is still in
    /// flight isn't swept). Covers BOTH the crash-between-the-flip-and-the-
    /// enqueue window AND a terminal-Failed create_boot whose fenced flip
    /// to Failed also errored — either way the session is stuck Pending
    /// holding a reservation with nothing to drive it (the deleted
    /// `requeue_stale_pending` was the old backstop). The reclaim sweep
    /// re-enqueues a fresh create_boot (the verb re-reads host_id from the
    /// row). Default: empty (mock stores).
    async fn orphaned_pending_sessions(
        &self,
        older_than: std::time::Duration,
    ) -> Result<Vec<SessionId>, MetaError> {
        let _ = older_than;
        Ok(Vec::new())
    }

    /// Fence-then-resume crash recovery: for every `running` op whose
    /// `heartbeat_at` is older than `stale`, CAS-bump the session's
    /// `current_epoch` again (the old writer is fenced everywhere) and
    /// re-stamp the row with the new epoch + `claimed_by`. Returns the
    /// re-claimed ops (each resumes at its recorded `step`).
    async fn op_reclaim_stale(
        &self,
        stale: std::time::Duration,
        claimed_by: &str,
    ) -> Result<Vec<crate::types::session_op::SessionOp>, MetaError> {
        let _ = (stale, claimed_by);
        Ok(Vec::new())
    }

    /// Uniform fenced session-row transition: `transition_session` with
    /// `AND current_epoch = $e`. `Ok(false)` = fenced (0 rows) — the
    /// caller stops silently, never retries, never compensates.
    async fn fenced_transition_session(
        &self,
        session_id: SessionId,
        epoch: i64,
        to: crate::types::SessionState,
    ) -> Result<Option<crate::types::SessionState>, MetaError> {
        let _ = (session_id, epoch, to);
        Err(MetaError::Serialization(
            "fenced writes not supported by this store".into(),
        ))
    }

    /// Fenced sandbox (re)bind — subsumes `rebind_session_guarded`'s
    /// bespoke expected-state list with the one epoch predicate.
    async fn fenced_assign_sandbox(
        &self,
        session_id: SessionId,
        epoch: i64,
        sandbox_id: Option<SandboxId>,
        host_id: Option<crate::types::HostId>,
    ) -> Result<bool, MetaError> {
        let _ = (session_id, epoch, sandbox_id, host_id);
        Err(MetaError::Serialization(
            "fenced writes not supported by this store".into(),
        ))
    }

    /// ADR 0073 phase 2: durably enqueue a command for delivery.
    /// Idempotent on `prompt_id` (INSERT … ON CONFLICT DO NOTHING) so a
    /// caller retry never duplicates a row. The Postgres impl also
    /// fires `pg_notify('session_outbox', session_id)` in the same
    /// round trip to wake every replica's delivery driver.
    ///
    /// Default (mock stores): drops the row — mocks get pre-0067
    /// fire-and-forget delivery semantics; tests that assert outbox
    /// behavior use a store that overrides these.
    async fn outbox_enqueue(&self, row: &crate::types::outbox::OutboxRow) -> Result<(), MetaError> {
        let _ = row;
        Ok(())
    }

    /// Sessions with at least one due, un-acked row (`acked_at IS NULL
    /// AND not_before <= now()`). The delivery driver fans out from
    /// this set. Default (mocks): empty.
    async fn outbox_due_sessions(&self) -> Result<Vec<SessionId>, MetaError> {
        Ok(Vec::new())
    }

    /// The OLDEST due, un-acked row for a session — per-session
    /// delivery order is row order (created_at). Default (mocks): none.
    async fn outbox_next_due(
        &self,
        session_id: SessionId,
    ) -> Result<Option<crate::types::outbox::OutboxRow>, MetaError> {
        let _ = session_id;
        Ok(None)
    }

    /// Record a relay handoff: stamp `delivered_at`, bump `attempts`,
    /// and push `not_before` to `now() + ack_timeout` so the row
    /// re-becomes due by itself if the confirming event never lands.
    async fn outbox_mark_delivered(
        &self,
        prompt_id: &str,
        ack_timeout: std::time::Duration,
    ) -> Result<(), MetaError> {
        let _ = (prompt_id, ack_timeout);
        Ok(())
    }

    /// Push a row's `not_before` out (delivery failed; retry later).
    async fn outbox_defer(
        &self,
        prompt_id: &str,
        delay: std::time::Duration,
    ) -> Result<(), MetaError> {
        let _ = (prompt_id, delay);
        Ok(())
    }

    /// Terminal ack: the confirming harness event was ingested.
    /// Returns whether a row was newly acked (false = unknown id or
    /// already acked — both fine; acks are at-least-once too).
    async fn outbox_ack(&self, prompt_id: &str) -> Result<bool, MetaError> {
        let _ = prompt_id;
        Ok(false)
    }

    /// Phase-1b type-ahead edit for a row the relay has NOT yet handed
    /// off (`delivered_at IS NULL AND acked_at IS NULL`): swap the
    /// prompt text in place. Returns false when no such row exists
    /// (the prompt already reached the harness queue — edit it there).
    async fn outbox_update_prompt_text(
        &self,
        prompt_id: &str,
        text: &str,
    ) -> Result<bool, MetaError> {
        let _ = (prompt_id, text);
        Ok(false)
    }

    /// Phase-1b dequeue for an undelivered row: delete it. Returns
    /// false when the row was already delivered/acked (dequeue via the
    /// harness queue instead).
    async fn outbox_delete_undelivered(&self, prompt_id: &str) -> Result<bool, MetaError> {
        let _ = prompt_id;
        Ok(false)
    }

    /// ADR 0045 C2: the `Committing` persist — rebind a session's host
    /// AND sandbox in one step. The ownership oracle
    /// (`sandbox_ownership`: `session.sandbox_id == sandbox`) must flip
    /// atomically with the rebind, which is the entire semantic content
    /// of post-copy ownership transfer. The default is the sequential
    /// two-step (mock/test stores); the Postgres store overrides with a
    /// single UPDATE.
    async fn rebind_session(
        &self,
        id: SessionId,
        host_id: HostId,
        sandbox_id: SandboxId,
    ) -> Result<(), MetaError> {
        self.assign_session_host(id, Some(host_id)).await?;
        self.assign_session_sandbox(id, Some(sandbox_id)).await
    }

    /// Issue #211: guarded compare-and-swap variant of
    /// [`Self::assign_session_sandbox`]. The three binding writers
    /// (`assign_session_{host,sandbox}`, `rebind_session`) are otherwise
    /// blind `WHERE id = $1` UPDATEs: a racing actor can bind a live
    /// sandbox onto a row that has *concurrently* gone terminal (the
    /// terminate-races-resume interleaving), defeating the orphan reap —
    /// the ownership oracle matches `sandbox_id` only and never re-checks
    /// status, so a live VM pinned to a `Completed` row leaks forever.
    ///
    /// This method conditions the write on:
    ///   * `expected_current` — `Some(prev)` requires the row's current
    ///     `sandbox_id` to equal `prev` (use `Some(None)` to require it
    ///     currently NULL, `Some(Some(s))` to require it equals `s`).
    ///     `None` means "don't compare the old sandbox".
    ///   * `allowed_states` — if non-empty, the row's `status` must be one
    ///     of these. Empty means "any state".
    ///
    /// Returns [`MetaError::Conflict`] when the guard rejects the write
    /// (0 rows matched but the row exists), [`MetaError::NotFound`] when no
    /// row with this id exists at all. Callers that just created a sandbox
    /// MUST destroy it on `Conflict` rather than leaking it.
    ///
    /// The default impl composes a `get_session` legality check with the
    /// blind setter — atomic enough for single-threaded mock stores; the
    /// Postgres store overrides it with a true single-statement CAS.
    async fn assign_session_sandbox_guarded(
        &self,
        id: SessionId,
        sandbox_id: Option<SandboxId>,
        expected_current: Option<Option<SandboxId>>,
        allowed_states: &[SessionState],
    ) -> Result<(), MetaError> {
        let session = self.get_session(id).await?;
        if let Some(expected) = expected_current {
            if session.sandbox_id != expected {
                return Err(MetaError::Conflict(format!(
                    "assign_session_sandbox guard: sandbox_id is {:?}, expected {:?}",
                    session.sandbox_id, expected
                )));
            }
        }
        if !allowed_states.is_empty() && !allowed_states.contains(&session.status) {
            return Err(MetaError::Conflict(format!(
                "assign_session_sandbox guard: status is {}, not in {:?}",
                session.status.as_str(),
                allowed_states
            )));
        }
        self.assign_session_sandbox(id, sandbox_id).await
    }

    /// Issue #211: guarded CAS variant of [`Self::assign_session_host`].
    /// See [`Self::assign_session_sandbox_guarded`] for the guard
    /// semantics; `expected_current` here is matched against the row's
    /// `sandbox_id` (the host bind in the resume path always lands paired
    /// with the sandbox bind, so the sandbox is the meaningful witness).
    async fn assign_session_host_guarded(
        &self,
        id: SessionId,
        host_id: Option<HostId>,
        expected_current: Option<Option<SandboxId>>,
        allowed_states: &[SessionState],
    ) -> Result<(), MetaError> {
        let session = self.get_session(id).await?;
        if let Some(expected) = expected_current {
            if session.sandbox_id != expected {
                return Err(MetaError::Conflict(format!(
                    "assign_session_host guard: sandbox_id is {:?}, expected {:?}",
                    session.sandbox_id, expected
                )));
            }
        }
        if !allowed_states.is_empty() && !allowed_states.contains(&session.status) {
            return Err(MetaError::Conflict(format!(
                "assign_session_host guard: status is {}, not in {:?}",
                session.status.as_str(),
                allowed_states
            )));
        }
        self.assign_session_host(id, host_id).await
    }

    /// Issue #211: guarded CAS variant of [`Self::rebind_session`]. The
    /// migration `Committing` persist replaces an *old* sandbox with a
    /// *new* one; `expected_current` (the old sandbox it read) ensures a
    /// reconcile strike-out or a competing rebind that already moved the
    /// row doesn't get clobbered. `allowed_states` keeps the rebind from
    /// landing on a row that went terminal mid-migration.
    async fn rebind_session_guarded(
        &self,
        id: SessionId,
        host_id: HostId,
        sandbox_id: SandboxId,
        expected_current: Option<Option<SandboxId>>,
        allowed_states: &[SessionState],
    ) -> Result<(), MetaError> {
        let session = self.get_session(id).await?;
        if let Some(expected) = expected_current {
            if session.sandbox_id != expected {
                return Err(MetaError::Conflict(format!(
                    "rebind_session guard: sandbox_id is {:?}, expected {:?}",
                    session.sandbox_id, expected
                )));
            }
        }
        if !allowed_states.is_empty() && !allowed_states.contains(&session.status) {
            return Err(MetaError::Conflict(format!(
                "rebind_session guard: status is {}, not in {:?}",
                session.status.as_str(),
                allowed_states
            )));
        }
        self.rebind_session(id, host_id, sandbox_id).await
    }

    /// ADR 0015 M3: PG-authoritative lookup for "which host owns this
    /// sandbox right now, and what state is its session in?"
    /// `HostRegistry` calls this on cache miss (or after the per-host
    /// TTL elapses) to repair stale `sandbox_owner` entries.
    ///
    /// Returns `(host_id, session_status)` for the session whose
    /// `sandbox_id` column matches, or `None` when no row points at
    /// this sandbox (already orphaned, never existed, or migrated
    /// away). The `host_id` reflects the row's *current* binding —
    /// callers should compare it against their in-memory cache and
    /// repair if drifted. `session_status` lets `HostRegistry` decide
    /// between `SandboxError::HostLost` (host known dead, 410) and
    /// `SandboxError::NotFound` (sandbox unknown, 404) without a
    /// second round-trip.
    ///
    /// Default scans `list_active_sessions` and filters in memory —
    /// fine for in-memory test mocks. Postgres overrides with a
    /// single-row indexed query.
    async fn host_for_sandbox(
        &self,
        sandbox_id: SandboxId,
    ) -> Result<Option<(HostId, SessionState)>, MetaError> {
        let all = self.list_active_sessions().await?;
        Ok(all
            .into_iter()
            .find_map(|s| match (s.host_id, s.sandbox_id) {
                (Some(h), Some(sb)) if sb == sandbox_id => Some((h, s.status)),
                _ => None,
            }))
    }

    // ---- hosts ----
    async fn upsert_host(&self, host: HostRecord) -> Result<(), MetaError>;
    async fn list_active_hosts(&self) -> Result<Vec<HostRecord>, MetaError>;
    async fn set_host_status(&self, id: HostId, status: HostStatus) -> Result<(), MetaError>;

    /// ADR 0068: `hosts.capabilities.fc_snapshot_version` for one host —
    /// the value the eviction pipeline and the checkpoint-advert
    /// reconcile stamp onto a freshly-recorded `snapshots` row so
    /// placement can later pair a restore against the exact FC
    /// snapshot-data-format version that captured it. Default derives
    /// from `list_active_hosts()` (an O(active hosts) scan is fine for
    /// a per-capture call, which already round-trips several times);
    /// `None` when the host isn't found or hasn't reported a version.
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

    /// Record a heartbeat from `host_id`: bump `last_heartbeat_at` to
    /// NOW(), set the host-reported `status`, and persist the full
    /// [`HostHeartbeat`] payload — capacity, utilization, and (ADR
    /// 0047) the scheduling state that used to live only in the
    /// per-pod in-memory mirror: `ready_images`, `local_snapshots`,
    /// `current_bundles`, `total_vcpus`. This is the single
    /// per-heartbeat `hosts` UPDATE; every coordinator replica
    /// schedules from these columns. Deliberately does NOT touch
    /// `cordoned` (coordinator-owned; see `set_host_cordoned`).
    /// Distinct from `set_host_status` because drain/dead transitions
    /// imply nothing about liveness and must not refresh the
    /// dead-host detector's timestamp.
    async fn touch_host_heartbeat(&self, id: HostId, hb: HostHeartbeat) -> Result<(), MetaError>;

    /// ADR 0047: flip the coordinator-owned `hosts.cordoned` bit.
    /// Written only by the admin cordon/uncordon endpoints and the
    /// ADR 0048 scale-down wave driver; heartbeats never touch it, so
    /// a cordon survives until an explicit uncordon. Returns
    /// `MetaError::NotFound` when no row exists — but succeeds for a
    /// host that has a row yet no live connection (wave-cordon during
    /// pod churn must stick).
    async fn set_host_cordoned(&self, id: HostId, cordoned: bool) -> Result<(), MetaError>;

    /// List hosts whose `last_heartbeat_at` is older than `threshold_secs`
    /// AND whose status is `Ready`. The dead-host detector polls this every
    /// ~10s and races other coordinator replicas via `pg_try_advisory_lock`
    /// for the right to evict each candidate. `Dead` rows are filtered out so
    /// a still-running replica's detector doesn't re-kill them; `Draining`
    /// rows are filtered out because they're operator-managed (mid image-roll
    /// — where ADR 0044 K2 reattach keeps the VMs alive across the pod-swap
    /// heartbeat gap — or mid node-removal), so the detector must not race the
    /// operator and route a reattaching host's sessions to Idle.
    async fn list_stale_hosts(&self, threshold_secs: u64) -> Result<Vec<HostRecord>, MetaError>;

    /// Atomically (a) mark `host_id` as `Dead`, (b) clear `host_id`
    /// and `sandbox_id` on every non-terminal session pointed at it,
    /// (c) transition those sessions to `HostLost`.
    ///
    /// ADR 0015 M2: the orphaned sessions move to `HostLost` rather
    /// than straight to `Dead`. The caller then runs a per-session
    /// snapshot check and drives the second-stage transition:
    /// `HostLost -> Idle` if a recoverable snapshot exists,
    /// `HostLost -> Dead` otherwise. Splitting the two stages makes
    /// "host went away" a distinct lifecycle moment from "the
    /// session is unrecoverable," which is what M4 (session
    /// migration) needs as an entry point.
    ///
    /// Returns `(SessionId, previous_state)` pairs so the caller can
    /// emit honest `StatusChanged { from: previous_state, to:
    /// HostLost }` events instead of hand-encoding a placeholder
    /// `from`. Postgres uses a single transaction; the Mock takes
    /// its sessions mutex once. Idempotent on a host already marked
    /// Dead — returns an empty vec since no sessions still point at
    /// it.
    async fn mark_host_dead_and_orphan_sessions(
        &self,
        host_id: HostId,
    ) -> Result<Vec<(SessionId, SessionState)>, MetaError>;

    // ---- snapshots ----
    /// ADR 0077 phase 3: persist a session's RuntimeSpec (upsert). Written
    /// in the create transaction (`reserve_and_persist_create`) and
    /// refreshed at eviction finalize; the boot inputs a queue re-prepare /
    /// resume / evac consumes instead of re-deriving. The SINGLE source of
    /// a session's selected skills/harness/workdir — it SUBSUMES #566's
    /// interim `sessions.selected_skills` column (retired, migration 0090).
    /// Default (mock): no-op.
    async fn put_session_runtime_spec(
        &self,
        session_id: SessionId,
        spec: &crate::types::runtime_spec::RuntimeSpec,
    ) -> Result<(), MetaError> {
        let _ = (session_id, spec);
        Ok(())
    }

    /// ADR 0077 phase 3: read a session's persisted RuntimeSpec. `None` =
    /// none written yet (pre-0074 sessions). Default (mock): `None`.
    async fn get_session_runtime_spec(
        &self,
        session_id: SessionId,
    ) -> Result<Option<crate::types::runtime_spec::RuntimeSpec>, MetaError> {
        let _ = session_id;
        Ok(None)
    }

    /// Idempotent upsert (`ON CONFLICT (id) DO UPDATE`) keyed by
    /// `snap.id`. Returns `true` iff this call INSERTed a fresh row,
    /// `false` on a re-record of an existing one — issue #529: the
    /// heartbeat reconcile uses this to emit `SnapshotTaken` exactly
    /// once, on the row's first landing. ADR 0077 phase 1: the SAME
    /// transaction advances the session's `durable_head`.
    async fn record_snapshot(&self, snap: SnapshotRecord) -> Result<bool, MetaError>;

    /// ADR 0079 (re-review findings #3/#4): the fenced counterpart to
    /// [`record_snapshot`]. Writes the row ONLY while the session's
    /// `current_epoch` still equals `epoch`, atomically in one transaction.
    /// Returns `Ok(false)` when the op executor has been fenced by a
    /// successor's re-claim (the epoch moved) — NOTHING is written, so a
    /// reclaimed-out predecessor can never land a phantom `recoverable` row
    /// a resume would later pick (the 89f7984d durability-lie class). The
    /// op-driven capture sites (idle eviction, manual snapshot) use this
    /// instead of the plain insert; the host-heartbeat reconcile and
    /// image-enable base captures keep [`record_snapshot`] (not op-fenced).
    ///
    /// The default delegates to [`record_snapshot`] (fence-less) — fine for
    /// in-memory test mocks that never exercise a reclaim; the Postgres
    /// impl overrides it with the real, atomic epoch gate.
    async fn fenced_record_snapshot(
        &self,
        snap: SnapshotRecord,
        epoch: i64,
    ) -> Result<bool, MetaError> {
        let _ = epoch;
        self.record_snapshot(snap).await
    }

    /// ADR 0077 phase 1: the session's durable head — the newest
    /// snapshot whose blobs AND row are both committed (advanced in
    /// `record_snapshot`'s transaction). `None` = no committed
    /// snapshot yet. Phase 5 makes this the resume selector; phase 1
    /// exposes it read-side for the SLO canary and tests.
    ///
    /// Default (mock stores): `None` — mocks get pre-0074 behavior.
    async fn durable_head_snapshot(
        &self,
        session_id: SessionId,
    ) -> Result<Option<crate::types::SnapshotId>, MetaError> {
        let _ = session_id;
        Ok(None)
    }
    async fn list_snapshots_for_session(
        &self,
        sid: SessionId,
    ) -> Result<Vec<SnapshotRecord>, MetaError>;
    async fn latest_snapshot_for_session(
        &self,
        sid: SessionId,
    ) -> Result<Option<SnapshotRecord>, MetaError>;
    /// ADR 0028 A.log: the session's `session_events.idx`
    /// high-water-mark at or before `at` — the event-log leg of a
    /// checkpoint's (memory, disk, event-log) coherence triple,
    /// resolved against the capture's pause instant. `None` when the
    /// session has no events yet (a cursor of "before everything").
    ///
    /// Default `Ok(None)` so mocks without an event log degrade to
    /// "no rewind information" rather than forcing every test double
    /// to model events.
    async fn latest_event_idx_at_or_before(
        &self,
        _sid: SessionId,
        _at: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<i64>, MetaError> {
        Ok(None)
    }
    /// ADR 0028 Fix A: delete session-bound snapshot rows older than
    /// `retention`, EXCEPT each session's latest (the rung-1 recovery
    /// anchor — never collectible while the session row exists).
    /// Template snapshots (`session_id IS NULL`) are exempt. Returns
    /// the ids of the deleted rows so the caller can also delete their
    /// portable `snapshots/<id>/` blobs (state.bin / sidecar) — those
    /// live outside the chunk-GC namespace and are otherwise never
    /// collected. Chunks the pruned rows exclusively referenced become
    /// GC candidates via the existing pin-set machinery.
    ///
    /// Default `Ok(vec![])` so mocks without a snapshots table skip it.
    async fn prune_session_snapshots(
        &self,
        _retention: chrono::Duration,
    ) -> Result<Vec<crate::types::SnapshotId>, MetaError> {
        Ok(Vec::new())
    }
    /// The `session_id IS NULL` mirror of [`Self::prune_session_snapshots`]:
    /// reap orphaned per-image *base* snapshots. Every image re-bake/refresh
    /// captures a fresh base and swaps `enabled_images.base_snapshot_id` to
    /// it (`upsert_enabled_image`'s `ON CONFLICT`), leaving the PRIOR base
    /// row dangling — `session_id IS NULL`, pointed to by no `enabled_images`
    /// row. Nothing else ever deletes it, and it keeps pinning its own
    /// disk+memory chunks via pin-set sources #3/#4
    /// (`list_recoverable_snapshot_{disk,memory}_manifests`, which filter on
    /// `recoverable = TRUE` with NO `session_id` predicate) — so without this
    /// reaper every refresh permanently leaks one base snapshot's chunks
    /// (20–32 GB for the heavy dogfood images).
    ///
    /// Deletes `session_id IS NULL` rows older than `grace` that are NOT
    /// referenced by any `enabled_images.base_snapshot_id` — including
    /// soft-deleted image rows, whose chunk lineage is intentionally still
    /// pinned (ADR 0021 P1.8). The `base_snapshot_id` FK
    /// (`REFERENCES snapshots(id)`, no `ON DELETE`) is a hard backstop: even
    /// a buggy predicate can't delete an in-use base. Bumps `chunk_generation`
    /// in the same TX (GC-barrier symmetry with `record_snapshot`); the
    /// existing chunk-GC + snapshot-blob-GC sweeps then reclaim the
    /// now-unpinned chunks and portable `snapshots/<id>/` blobs. Returns the
    /// deleted ids (count is the only consumer today).
    ///
    /// Default `Ok(vec![])` so mocks without a snapshots table skip it.
    async fn prune_orphan_base_snapshots(
        &self,
        _grace: chrono::Duration,
    ) -> Result<Vec<crate::types::SnapshotId>, MetaError> {
        Ok(Vec::new())
    }
    /// ADR 0014 M1.11: fetch a single snapshot row by id. Used by
    /// the heartbeat-ack template enrichment path to surface the
    /// snapshot's persisted `disk_manifest` + `memory_manifest`
    /// to host-agents — without those, a base-snapshot restore
    /// on a fresh host has no way to materialize the rootfs file
    /// FC `load_snapshot` needs.
    ///
    /// Default returns `None` so backends that don't have a real
    /// DB (mocks, tests) opt out cleanly; callers that depend on
    /// the persisted shape (heartbeat enrichment) override.
    async fn get_snapshot(
        &self,
        _id: crate::types::SnapshotId,
    ) -> Result<Option<SnapshotRecord>, MetaError> {
        Ok(None)
    }

    /// Enumerate every disk `manifest_id` referenced by a live
    /// snapshot row. Today's sole caller is `reap_materialize_dir`,
    /// which uses the result as the "do not delete" filter when
    /// pruning assembled `<manifest_id>-vN.ext4` files on hosts.
    /// (The chunk-store GC that previously consumed this set was
    /// removed 2026-05-23 — see ADR 0015 M5.)
    ///
    /// Returns DISTINCT ids; versions aren't surfaced. Default
    /// `Ok(vec![])` keeps in-memory test impls quiet — the real
    /// query lives in `engram-postgres`.
    async fn list_live_disk_manifest_ids(&self) -> Result<Vec<uuid::Uuid>, MetaError> {
        Ok(Vec::new())
    }

    /// Symmetric memory-side companion to
    /// `list_live_disk_manifest_ids`. Currently unused by any
    /// in-tree caller after the chunk-store GC was removed, but
    /// kept on the trait so a future reintroduction has a
    /// pre-shaped seam.
    async fn list_live_memory_manifest_ids(&self) -> Result<Vec<uuid::Uuid>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0016 Phase C: pin-set source #3 — every recoverable
    /// snapshot's chunked-disk `ManifestRef` (id + version). Unlike
    /// `list_live_disk_manifest_ids` (which returns Uuid-only for
    /// the `reap_materialize_dir` reaper), the pin-set query needs
    /// the version so `chunk_store.get_manifest` reads the exact
    /// manifest that snapshot row depends on.
    ///
    /// Filtered to `recoverable=true` per the ADR's pin-set
    /// definition: non-recoverable snapshot rows are informational
    /// (bake-time / soft-evicted) and their chunks shouldn't keep
    /// BlobStorage growing forever.
    ///
    /// Default `Ok(vec![])` keeps mocks quiet; PG override returns
    /// DISTINCT (id, version) tuples wrapped as `ManifestRef`.
    async fn list_recoverable_snapshot_disk_manifests(
        &self,
    ) -> Result<Vec<ManifestRef>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0016 Phase C: pin-set source #4 — every recoverable
    /// snapshot's chunked-memory `ManifestRef` (id + version).
    /// Memory-side mirror of [`list_recoverable_snapshot_disk_manifests`].
    /// VZ snapshots (no chunked memory) drop out via the partial
    /// index on `memory_manifest_id`.
    async fn list_recoverable_snapshot_memory_manifests(
        &self,
    ) -> Result<Vec<ManifestRef>, MetaError> {
        Ok(Vec::new())
    }

    // ---- session event log ----

    /// Append an event to a session's persistent log. Returns the
    /// monotonic per-session `idx` assigned to this event. Allocation
    /// is atomic — concurrent appends to the same session never
    /// collide and never leave gaps. `kind` is the event discriminant
    /// (e.g. "stdout", "snapshot_taken"); `payload` is the wire JSON.
    async fn append_session_event(
        &self,
        session_id: SessionId,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<i64, MetaError>;

    /// ADR 0079 (review finding #6): append a lifecycle event ONLY when
    /// `sessions.current_epoch == epoch`, atomically (the epoch predicate
    /// rides the same idx-allocation UPDATE). `Ok(None)` ⇒ a successor op
    /// re-claimed the session — the caller is a fenced-out predecessor and
    /// its stale StatusChanged/Evicted must NOT land after the successor's
    /// newer events (an event-log tail corruption that misleads
    /// SSE/idle-detect/transcript). The default appends unconditionally
    /// (benign for non-fencing mock stores); `PostgresStore` and the
    /// coordinator's `MiniMeta` enforce the fence.
    async fn append_session_event_fenced(
        &self,
        session_id: SessionId,
        epoch: i64,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<Option<i64>, MetaError> {
        let _ = epoch;
        self.append_session_event(session_id, kind, payload)
            .await
            .map(Some)
    }

    /// Phase 1c (ADR 0052): fan out one EPHEMERAL session event (a live
    /// token chunk) to all coordinator replicas via
    /// `NOTIFY session_event_deltas`. Unlike [`append_session_event`],
    /// this NEITHER persists a row NOR allocates an `idx` — the full event
    /// rides inline in the notification payload so every replica's
    /// `pg_listener` can re-broadcast it to its local SSE bus without a DB
    /// fetch. `payload` is the serialized [`SessionEvent`]. Best-effort by
    /// contract: a dropped notification costs only live animation, never
    /// correctness (the durable terminal message is the record).
    ///
    /// Default no-op so non-Postgres / mock stores simply don't stream;
    /// only [`PostgresStore`](../../../engram_postgres) overrides it.
    async fn notify_session_delta(
        &self,
        _session_id: SessionId,
        _payload: &serde_json::Value,
    ) -> Result<(), MetaError> {
        Ok(())
    }

    /// Return events with `idx > since`, in idx order, capped at
    /// `limit`. Used by `GET /sessions/:id/events?since=N` (and by
    /// EventSource auto-reconnect via `Last-Event-ID`) to replay the
    /// log before tailing the live bus. Pass `since = -1` to start
    /// from the very first event.
    async fn list_session_events_since(
        &self,
        session_id: SessionId,
        since: i64,
        limit: i64,
    ) -> Result<Vec<PersistedEvent>, MetaError>;

    /// The NEWEST `limit` events, returned in ASCENDING idx order (the
    /// same shape `list_session_events_since` yields, just anchored at
    /// the tail). What every interactive consumer wants; the forward
    /// cursor has no way to express it (2026-07-11 campaign: agents
    /// polling long sessions with oldest-N reads stalled repeatedly).
    /// Default impl (mocks): delegate to the forward read — correct for
    /// stores whose event count fits the caller's limit anyway.
    async fn list_session_events_tail(
        &self,
        session_id: SessionId,
        limit: i64,
    ) -> Result<Vec<PersistedEvent>, MetaError> {
        self.list_session_events_since(session_id, -1, limit).await
    }

    /// ADR 0028 A.log: rung-1 recovery rewind. Tombstone every live
    /// event with `idx > events_cursor` (set `rewound_at = now()`),
    /// bump the session's `recovery_epoch`, and return a
    /// [`RewindSummary`] (rolled-back count + the surviving
    /// outside-world side-effects detected in that span). Idempotent
    /// in spirit: if nothing is past the cursor, returns
    /// `rolled_back == 0` and the caller emits no boundary.
    ///
    /// Default `Ok(RewindSummary::default())` so mocks without an
    /// event log are a clean no-op.
    async fn rewind_session_to_cursor(
        &self,
        _session_id: SessionId,
        _events_cursor: i64,
    ) -> Result<crate::types::event::RewindSummary, MetaError> {
        Ok(crate::types::event::RewindSummary::default())
    }

    /// Issue #527 Phase 1: resolve the prompt→run-start latency for a given
    /// `prompt_id` — the coordinator-authoritative "the user asked at time
    /// T" receipt's age, written as the first PG side-effect of
    /// `send_prompt_core` (before auto-resume). Used by the harness-event
    /// sink to record `engram_prompt_to_run_started_seconds` when the
    /// matching `HarnessRunStarted{prompt_id}` lands. Returns `Ok(None)`
    /// when no receipt exists — the env-seeded initial prompt carries no
    /// `prompt_id` and never gets one, so this is an expected, non-error
    /// case the caller skips silently rather than treating as a bug.
    ///
    /// PR #556 review finding #1: the elapsed seconds are computed
    /// PG-side (`NOW() - created_at`, one clock) rather than by handing
    /// the receipt's `created_at` back for the caller to diff against a
    /// coordinator-process `Utc::now()` — mixing those two clocks biases
    /// (or, under skew, silently drops) exactly the samples this metric
    /// exists to capture.
    ///
    /// Default `Ok(None)` so mocks without an event log are a clean no-op
    /// (they simply never emit the derived histogram).
    async fn prompt_received_seconds_ago(
        &self,
        _session_id: SessionId,
        _prompt_id: &str,
    ) -> Result<Option<f64>, MetaError> {
        Ok(None)
    }

    // ---- file artifacts (ADR 0026) ----

    /// Record a shared file artifact for a session. `id` is the
    /// server-generated UUID that also names the blob key; the row
    /// cascades on session delete. `caption` is untrusted text the
    /// caller has already length-capped + control-char-stripped.
    async fn insert_artifact(
        &self,
        id: uuid::Uuid,
        session_id: SessionId,
        blob_key: &str,
        media_type: &str,
        size_bytes: i64,
        caption: Option<&str>,
    ) -> Result<(), MetaError>;

    /// Fetch one artifact **scoped to its session** (so one session can
    /// never read another's blob key). `None` if the `(session_id, id)`
    /// pair doesn't exist. Backs the serve endpoint
    /// `GET /sessions/:id/artifacts/:artifact_id`.
    async fn get_artifact(
        &self,
        session_id: SessionId,
        id: uuid::Uuid,
    ) -> Result<Option<ArtifactRow>, MetaError>;

    /// Per-session artifact usage `(count, total_bytes)` for the upload
    /// quota check. A fresh session with no artifacts returns `(0, 0)`.
    async fn artifact_usage(&self, session_id: SessionId) -> Result<(i64, i64), MetaError>;

    // ---- registry credentials (Phase 5) ----

    /// Insert or replace a registry credential row. The
    /// `wrapped_dek` / `nonce` / `ciphertext` come from
    /// `engram-crypto::CredCipher::seal`. `(registry_host, username)`
    /// is the natural key — re-adding the same pair updates the
    /// cipher fields in place.
    async fn upsert_registry_credential(&self, cred: RegistryCredential) -> Result<(), MetaError>;

    async fn list_registry_credentials(&self) -> Result<Vec<RegistryCredential>, MetaError>;

    /// Look up the credential row for a given registry host. Returns
    /// `None` when no row exists — callers fall back to anonymous
    /// access (works for public registries and `localhost:5001`).
    /// When more than one row exists for the same host, returns the
    /// most-recently-updated row.
    async fn registry_credential_for_host(
        &self,
        registry_host: &str,
    ) -> Result<Option<RegistryCredential>, MetaError>;

    async fn delete_registry_credential(&self, registry_host: &str) -> Result<(), MetaError>;

    // ADR 0021 P1.5a retired the harness-pack registry methods that
    // used to live here. The harness is an image property baked at
    // image-bake time now; there's no deployment-wide table for the
    // MetadataStore to vend.

    // ---- enabled images (Phase 5b) ----
    //
    // Curated allowlist of image URIs that sessions may reference.
    // Manifest is fetched at enable time and persisted on the row,
    // so session-create has zero network dependency on the manifest
    // path. The host-agent still pulls the rootfs blob on first use,
    // but that's lazy + cached separately by digest.

    async fn upsert_enabled_image(&self, image: EnabledImage) -> Result<(), MetaError>;
    /// ADR 0080 cheap-edit path: replace `image_config` on a live row
    /// WITHOUT touching the base snapshot. Only correct for edits
    /// that don't affect capture (name/description/env/workdir —
    /// callers gate resources/warm changes behind a recapture job).
    /// Implementations must notify boot-bundle-cache listeners so
    /// coordinator replicas drop their cached copy. `NotFound` when
    /// no live row exists for `image_uri`.
    async fn update_enabled_image_config(
        &self,
        image_uri: &str,
        config: &crate::types::image::ImageConfig,
    ) -> Result<(), MetaError> {
        let _ = (image_uri, config);
        Err(MetaError::Migration(
            "enabled-image config updates unsupported by this store".into(),
        ))
    }
    /// Live-only — filters `soft_deleted_at IS NULL`. Used by host
    /// advertisement, the dashboard's enabled-images list, and the
    /// session-create handler (where a disabled image must fail with
    /// "not enabled" rather than silently start a session against a
    /// deprecated image).
    async fn list_enabled_images(&self) -> Result<Vec<EnabledImage>, MetaError>;
    /// Live-only lookup (`soft_deleted_at IS NULL`). Use this for
    /// the session-create path; resume callers should use
    /// [`Self::get_enabled_image_any`].
    async fn get_enabled_image(&self, image_uri: &str) -> Result<Option<EnabledImage>, MetaError>;
    /// ADR 0021 P1.8: resume-path lookup. Returns the row even if
    /// `soft_deleted_at` is set, so a session whose image was
    /// disabled while it was idle can still resume from the
    /// (chunks-still-pinned) lineage. New-session callers should
    /// use [`Self::get_enabled_image`] (live-filtered).
    async fn get_enabled_image_any(
        &self,
        image_uri: &str,
    ) -> Result<Option<EnabledImage>, MetaError>;
    /// ADR 0021 P1.8: guarded soft-delete. Inside one transaction:
    /// lock the `enabled_images` row, count sessions in
    /// `{pending, created, active, evacuating}` referencing the
    /// image, return [`DisableEnabledImageOutcome::Blocked`] (with
    /// up to 16 blocking sessions) when nonzero, else flip
    /// `soft_deleted_at = NOW()` and return
    /// [`DisableEnabledImageOutcome::Disabled`]. Idempotent: a
    /// second disable of an already-soft-deleted row returns
    /// [`DisableEnabledImageOutcome::AlreadyDisabled`] without
    /// re-bumping the timestamp.
    async fn soft_delete_enabled_image(
        &self,
        image_uri: &str,
    ) -> Result<DisableEnabledImageOutcome, MetaError>;
    /// ADR 0021 P1.8: physical delete reserved for the future
    /// chunk-GC sweeper. Routine "disable" goes through
    /// [`Self::soft_delete_enabled_image`]; only call this when the
    /// row's chunks have been confirmed unreferenced and removed
    /// from BlobStorage.
    async fn delete_enabled_image(&self, image_uri: &str) -> Result<(), MetaError>;

    /// ADR 0036 P4 / ADR 0080: content-keyed base-snapshot reuse.
    /// Find an enabled image (INCLUDING soft-deleted rows — their
    /// snapshots stay GC-pinned and restorable) whose bake produced
    /// the same disk content (`disk_manifest_*`, content-derived
    /// since ADR 0036) AND the same capture-affecting `resources`
    /// (the only non-warm config that's frozen into the memory
    /// snapshot — ADR 0080 keys reuse on `image_config->'resources'`
    /// alone; name/description/env/workdir are applied per-session),
    /// and which carries a base snapshot. The enable pipeline reuses
    /// that snapshot instead of booting a capture VM: with both
    /// inputs equal, a fresh capture is equivalent for every session
    /// created from it (bundle generations are swapped to the host's
    /// current staging at session create — ADR 0035 Invariant 2 — so
    /// reuse does not freeze bundle freshness). Warm images never
    /// reuse (gated by the caller — a warm hook makes captures
    /// non-equivalent by definition).
    ///
    /// Default `None`: stores without the query surface (test mocks)
    /// simply never reuse.
    async fn find_enabled_image_by_content(
        &self,
        disk_manifest: ManifestRef,
        resources: &crate::types::image::ResourceHints,
    ) -> Result<Option<EnabledImage>, MetaError> {
        let _ = (disk_manifest, resources);
        Ok(None)
    }

    // ---- enable jobs (ADR 0036) ----
    //
    // Async image-enable state machine: `POST /api/enabled-images`
    // records a row and returns 202; the coordinator's
    // `enable_scanner` claims jobs via a lease and drives
    // `pending → materializing → capturing → ready | failed`.
    // Default implementations error — only the Postgres store (the
    // one the scanner runs against) supports jobs; the many test
    // mocks of this trait don't need to stub them out.

    /// Insert a `pending` job for `image_uri`, or — when a
    /// non-terminal job for the same URI already exists (the partial
    /// unique index) — return that job instead. Re-POSTing an
    /// in-flight enable is a resume/no-op, never duplicate work.
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
        image_config: &crate::types::image::ImageConfig,
    ) -> Result<EnableJob, MetaError> {
        let _ = (image_uri, manifest_digest, image_config);
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// Same as [`Self::create_or_get_enable_job`], with the async job marked
    /// to bypass base-snapshot reuse and force a fresh capture. Stores that
    /// have not added the column fall back to the normal enqueue path.
    async fn create_or_get_enable_job_with_options(
        &self,
        image_uri: &str,
        manifest_digest: Option<&str>,
        image_config: &crate::types::image::ImageConfig,
        force_recapture: bool,
    ) -> Result<EnableJob, MetaError> {
        let _ = force_recapture;
        self.create_or_get_enable_job(image_uri, manifest_digest, image_config)
            .await
    }

    async fn get_enable_job(&self, id: uuid::Uuid) -> Result<Option<EnableJob>, MetaError> {
        let _ = id;
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// Most-recent-first, bounded. The dashboard's images panel reads
    /// this to surface in-flight + recently-finished enables.
    async fn list_enable_jobs(&self, limit: u32) -> Result<Vec<EnableJob>, MetaError> {
        let _ = limit;
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// Atomically claim up to `limit` non-terminal jobs whose lease
    /// is free or expired (`claimed_at < now() - lease_secs`),
    /// stamping `(claimed_by, claimed_at)`. Multi-coordinator safety:
    /// one pod owns a job at a time; a crashed pod's claim expires
    /// and a peer re-claims. Progress checkpoints renew the claim.
    async fn claim_enable_jobs(
        &self,
        claimant: &str,
        lease_secs: u32,
        limit: u32,
    ) -> Result<Vec<EnableJob>, MetaError> {
        let _ = (claimant, lease_secs, limit);
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// Checkpoint materialize progress. Also renews the claim
    /// (`claimed_at = NOW()`) so a long materialize isn't stolen by
    /// a peer mid-run. `chunks_total` is stamped on first call.
    ///
    /// Fenced by `claimant`: the write only lands if the row's
    /// `claimed_by` still equals the caller. A pod whose lease has
    /// expired and been re-claimed by a peer gets
    /// [`MetaError::Conflict`] and must abandon the job (its renewal
    /// would otherwise reset the new claimant's progress and extend
    /// the lease on the wrong pod's behalf). `NotFound` only for a
    /// genuinely absent row.
    async fn update_enable_job_progress(
        &self,
        id: uuid::Uuid,
        claimant: &str,
        chunks_done: u32,
        chunks_total: Option<u32>,
    ) -> Result<(), MetaError> {
        let _ = (id, claimant, chunks_done, chunks_total);
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// ADR 0080 phase 3b: persist one `MaterializeProgress` frame from
    /// the streaming `MaterializeImage` RPC onto the job row — the
    /// `materializing`-stage counterpart of the ADR 0084 P1b
    /// `mirror_capture_progress_to_enable_job` (the capture-phase
    /// verb's fenced, claim-renewing predecessor,
    /// `update_enable_job_capture_progress`, was DELETED as dead code in
    /// ADR 0084 P4 — its only caller, the enable-scanner's old capture-
    /// progress consumer task, was removed in P1b when capture became a
    /// heartbeat-dispatched job). Renders the frame into
    /// `output_tail` (`materialize[<stage>] <detail>` — the job's
    /// operator-facing progress line) and ALSO renews the claim
    /// (`claimed_at = NOW()`), so the host's ≤30 s keepalive carries
    /// the lease exactly like capture frames do.
    ///
    /// Fenced by `claimant` — see [`Self::update_enable_job_progress`].
    ///
    /// `stages` is the scanner-maintained materialize stage timeline
    /// (ADR 0088 UI follow-up — the caller advances it per frame via
    /// pure logic and passes the whole array; this write replaces the
    /// column). Chunk-stage frames also carry window counts, persisted
    /// into `chunks_done`/`chunks_total` for the operator progress bar.
    async fn update_enable_job_materialize_progress(
        &self,
        id: uuid::Uuid,
        claimant: &str,
        progress: &crate::types::MaterializeProgress,
        stages: &[crate::types::WarmStageRecord],
    ) -> Result<(), MetaError> {
        let _ = (id, claimant, progress, stages);
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// ADR 0088: durable materialize placement — stamp the host this
    /// claimed job's materialize is about to run on, BEFORE the streaming
    /// RPC starts (no window where work runs unattributed). Liveness is
    /// derived, never stored: the binding counts as live only while
    /// `state = 'materializing'` and the claim is fresh (the keepalive
    /// frames renew it via
    /// [`Self::update_enable_job_materialize_progress`]). Never cleared —
    /// inert outside `materializing`, and a useful "where did the last
    /// materialize run" breadcrumb.
    ///
    /// Fenced by `claimant` — see [`Self::update_enable_job_progress`].
    async fn set_enable_job_materialize_host(
        &self,
        id: uuid::Uuid,
        claimant: &str,
        host: HostId,
    ) -> Result<(), MetaError> {
        let _ = (id, claimant, host);
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// ADR 0088: per-host in-flight enable work — the operator roll/drain
    /// gates' input, surfaced on the fleet view. `materialize_lease` is
    /// the claim-freshness window (the enable scanner's `lease_secs`): a
    /// materialize whose stream died stops renewing its claim and ages
    /// out of this map within the window. Hosts with no live work are
    /// absent. Default impl (mocks): empty map.
    async fn live_enable_work_by_host(
        &self,
        materialize_lease: std::time::Duration,
    ) -> Result<std::collections::HashMap<HostId, crate::types::LiveEnableWork>, MetaError> {
        let _ = materialize_lease;
        Ok(std::collections::HashMap::new())
    }

    /// Move the job's state forward (also renews the claim, clears
    /// `error` on non-failed targets, and releases the claim on
    /// terminal states).
    ///
    /// Fenced by `claimant` — see [`Self::update_enable_job_progress`].
    /// A stale pod must not be able to flip the state (e.g. drive a
    /// job the new claimant is actively completing back through
    /// `materializing`), nor release a claim it no longer holds.
    /// Returns [`MetaError::Conflict`] when the lease has moved on.
    async fn set_enable_job_state(
        &self,
        id: uuid::Uuid,
        claimant: &str,
        state: EnableJobState,
    ) -> Result<(), MetaError> {
        let _ = (id, claimant, state);
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// Record a pipeline failure in ONE atomic, fenced write: bump
    /// `attempts`, store `error`, release the claim (so any pod's next
    /// tick can retry), and flip to `failed` iff the retry budget is
    /// spent (`attempts + 1 >= max_attempts`) OR `force_terminal` is set
    /// (a deterministic, non-retryable failure — e.g. a `[warm]` hook
    /// that exits non-zero; retrying just re-loads the image for nothing).
    /// Returns the post-bump attempt count and the RESULTING state.
    ///
    /// The flip MUST happen here, not in a follow-up `set_enable_job_state`:
    /// this call releases the claim (`claimed_by → NULL`), so a separate
    /// fenced state write would fence-miss (`claimed_by` no longer matches)
    /// and silently fail — leaving the job non-terminal forever, re-claimed
    /// and re-failed every tick (the runaway-attempts bug).
    ///
    /// Fenced by `claimant` — see [`Self::update_enable_job_progress`].
    /// A stale pod's transient error must not clear the new claimant's
    /// lease or stamp `error` on a job that pod is actively completing.
    /// Returns [`MetaError::Conflict`] when the lease has moved on.
    async fn record_enable_job_failure(
        &self,
        id: uuid::Uuid,
        claimant: &str,
        error: &str,
        max_attempts: u32,
        force_terminal: bool,
    ) -> Result<(u32, EnableJobState), MetaError> {
        let _ = (id, claimant, error, max_attempts, force_terminal);
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// Admin retry: `failed → pending`, resetting attempts/error/
    /// claim. Errors `NotFound` for unknown ids; `Conflict` when the
    /// job isn't in `failed`.
    async fn retry_enable_job(&self, id: uuid::Uuid) -> Result<EnableJob, MetaError> {
        let _ = id;
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// ADR 0084 P1b: release the claim WITHOUT touching `state`/`attempts`/
    /// `error` — the watch-only exit for a `Capturing` job whose
    /// `capture_jobs` row is still in flight (`assigned`/`booting`/
    /// `warming`/`freezing`) or whose retryable failure was just
    /// reassigned to a fresh host+epoch. Unlike letting the claim simply
    /// age out (`claim_enable_jobs`'s `lease_secs`, 300s by default), this
    /// makes the job IMMEDIATELY re-claimable next tick (~3s later) — the
    /// job row's own state/progress is the durable source of truth, the
    /// enable-job claim only dedups short scanner ops, so there's no
    /// reason to make watch-only polling wait out a lease meant to bound
    /// a crashed pod's ownership. Fenced by `claimant`: `Ok(false)` means
    /// the lease had already moved (a peer's tick beat this one to it) —
    /// harmless, the peer's next tick observes the same row.
    async fn release_enable_job_claim(
        &self,
        id: uuid::Uuid,
        claimant: &str,
    ) -> Result<bool, MetaError> {
        let _ = (id, claimant);
        Ok(true)
    }

    // ---- ADR 0036 amendment: fleet chunk prestage (issue #538) ----
    //
    // A fourth, non-terminal enable-job stage between `capturing` and
    // `ready`: the scanner advertises the freshly-captured base snapshot
    // as a `prestage_images` heartbeat-ack entry and waits for every
    // eligible (`stages_images`) host to report the digest in
    // `ready_images` before the `enabled_images` upsert makes it visible
    // to session-create. See `docs/adr/0036-*.md`'s "prestage stage
    // (interim)" amendment.

    /// Stamp the wire-shape prestage ref (a JSON-encoded
    /// `engram_protocol::heartbeat::EnabledImageRef` — this trait can't
    /// depend on the protocol crate, so the caller serializes it) and flip
    /// the job to `Prestaging` in ONE fenced write (renews the claim, same
    /// as `set_enable_job_state`).
    ///
    /// Fenced by `claimant` — see [`Self::update_enable_job_progress`].
    /// Returns [`MetaError::Conflict`] when the lease has moved on.
    async fn begin_enable_job_prestage(
        &self,
        id: uuid::Uuid,
        claimant: &str,
        prestage_ref: serde_json::Value,
    ) -> Result<(), MetaError> {
        let _ = (id, claimant, prestage_ref);
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// Record the per-host prestage outcome map (`{"<host-uuid>":
    /// {"outcome": "staged"|"timed_out"|"unschedulable", "waited_ms": u64}}`),
    /// written once at the end of the prestage wait — the audit /
    /// dashboard record.
    ///
    /// Fenced by `claimant` — see [`Self::update_enable_job_progress`].
    /// Returns [`MetaError::Conflict`] when the lease has moved on.
    async fn set_enable_job_prestage_hosts(
        &self,
        id: uuid::Uuid,
        claimant: &str,
        outcomes: serde_json::Value,
    ) -> Result<(), MetaError> {
        let _ = (id, claimant, outcomes);
        Err(MetaError::Migration(
            "enable jobs unsupported by this store".into(),
        ))
    }

    /// The `prestage_ref` of every job currently in `prestaging`, raw JSON
    /// (the coordinator's heartbeat-ack builder deserializes each into
    /// `engram_protocol::heartbeat::EnabledImageRef` — this trait doesn't
    /// depend on the protocol crate, matching the existing dependency
    /// direction rather than laundering a stringly type). Read on every
    /// heartbeat ack; best-effort on the caller's side.
    async fn list_prestaging_refs(&self) -> Result<Vec<serde_json::Value>, MetaError> {
        Ok(Vec::new())
    }

    // ---- capture jobs (ADR 0084) ----
    //
    // Capture becomes a durable, host-executed, epoch-fenced job row
    // dispatched/reported over the heartbeat, replacing the
    // connection-coupled `BuildBaseSnapshot` RPC stream — a dropped
    // stream today keeps the capture running detached host-side while
    // the coordinator re-drives from scratch, booting a duplicate
    // capture VM with no anti-affinity. Every write below is fenced by
    // `(id, epoch)`, never a lease-holder identity: any coordinator
    // replica can record a host's report. Default implementations
    // error exactly like the enable-jobs family above (only the
    // Postgres store — the one hosts actually dispatch against —
    // supports jobs); the heartbeat-ack-adjacent bulk reads default to
    // empty, matching `list_prestaging_refs`/the GC pin-set reads,
    // since those are called unconditionally every tick regardless of
    // whether any capture jobs exist.
    //
    // This commit (P1a) only adds the store surface — dormant until
    // the executor/scanner rework (a later commit) calls it.

    /// Insert a fresh `assigned`-stage job for `row.enable_job_id`, or —
    /// when a non-terminal job for the same enable job already exists
    /// (the `capture_jobs_active_enable` partial unique index) — return
    /// that job instead (insert-or-get, exactly like
    /// [`Self::create_or_get_enable_job`]). A coordinator restart or a
    /// re-driven scanner tick must resume the existing attempt, never
    /// duplicate a capture VM.
    async fn insert_capture_job(&self, row: NewCaptureJob) -> Result<CaptureJobRow, MetaError> {
        let _ = row;
        Err(MetaError::Migration(
            "capture jobs unsupported by this store".into(),
        ))
    }

    /// One capture job by id.
    async fn get_capture_job(&self, id: CaptureJobId) -> Result<Option<CaptureJobRow>, MetaError> {
        let _ = id;
        Err(MetaError::Migration(
            "capture jobs unsupported by this store".into(),
        ))
    }

    /// ADR 0084 P1b: the MOST RECENT capture job for an enable job,
    /// terminal or not — deliberately NOT filtered to non-terminal rows
    /// (a filtered read would hide a row the instant it goes
    /// `done`/`failed`); this is
    /// what the enable scanner's watch-only `Capturing` arm polls: the
    /// tick that observes a fresh `done` (to finalize + advance to
    /// `prestaging`) or a terminal `failed` (to reassign-under-budget or
    /// bail non-retryable) needs to see that terminal row, not `None`.
    /// `ORDER BY created_at DESC LIMIT 1` — an enable job has at most one
    /// non-terminal capture job at a time (the partial unique index), but
    /// may accumulate multiple TERMINAL rows across retries of the
    /// enable job itself (`retry_enable_job`); the newest is authoritative.
    async fn latest_capture_job_for_enable(
        &self,
        enable_job_id: uuid::Uuid,
    ) -> Result<Option<CaptureJobRow>, MetaError> {
        let _ = enable_job_id;
        Err(MetaError::Migration(
            "capture jobs unsupported by this store".into(),
        ))
    }

    /// The fenced write every [`CaptureJobReport`] drives: advances
    /// `stage`/`stage_progress`/`last_progress_at` (bumping
    /// `stage_started_at` only when `stage` actually changes),
    /// `COALESCE`s in `fc_snapshot_version` once known, and — when
    /// `report.terminal` is `Some` — stamps `stage = 'done'` +
    /// `result_bincode`, or `stage = 'failed'` + `error`/`error_stage`/
    /// `retryable`. Fenced `WHERE id = $1 AND epoch = $2 AND stage NOT
    /// IN ('done', 'failed')` — any replica can perform this write, no
    /// lease-holder identity to lose. Returns whether the row was
    /// updated: `false` means the report is fenced off (a stale epoch
    /// from a reassigned-away attempt) or the job was already terminal
    /// — either way the caller drops the report, it must never retry
    /// or surface an error for it.
    async fn record_capture_job_report(
        &self,
        report: &CaptureJobReport,
    ) -> Result<bool, MetaError> {
        let _ = report;
        Err(MetaError::Migration(
            "capture jobs unsupported by this store".into(),
        ))
    }

    /// ADR 0084 (c): the reserving pick for a WAITING job (`host_id
    /// NULL`) — its fresh-insert placement and its every-tick re-attempt.
    /// Runs the SAME `FOR UPDATE` 2D best-fit as session placement
    /// (`pick_host_2d`, reserved-SUM now reading `capture_jobs`) over
    /// `candidates` (the coordinator's disk-footprint + anti-affinity +
    /// fc-version-filtered set) using the row's OWN stamped budgets. On a
    /// fit: binds `host_id` and clears `waiting_since` (dispatchable from
    /// the next heartbeat). On no fit: leaves the row waiting, stamping
    /// `waiting_since` on the first miss (`COALESCE` — restart-proof queue
    /// anchor). Idempotent + atomic: locks the job row `FOR UPDATE`, so a
    /// row already bound returns unchanged (no double-reserve). `None`
    /// when the job is gone or already terminal.
    async fn place_capture_job(
        &self,
        id: CaptureJobId,
        candidates: &[HostId],
    ) -> Result<Option<CaptureJobRow>, MetaError> {
        let _ = (id, candidates);
        Err(MetaError::Migration(
            "capture jobs unsupported by this store".into(),
        ))
    }

    /// Reassign a stalled DISPATCHED job (its `assigned`/stage deadline
    /// missed): fenced, epoch-bumped, re-running the reserving 2D fit over
    /// `candidates` in the SAME transaction so the host old→new swap is
    /// atomic with the reservation. On a fit: `host_id = $new`; on no fit:
    /// `host_id = NULL` (the row falls into the waiting flow rather than
    /// failing outright). Always `epoch = epoch + 1` (fences/tears down
    /// the abandoned attempt via host-side `cancel_absent`) and `attempts
    /// = attempts + 1` (a real re-attempt). Fenced `WHERE id = $1 AND
    /// epoch = $2 AND stage NOT IN ('done', 'failed')`; `None` when the
    /// fence missed (already reassigned, or terminal).
    async fn reassign_capture_job(
        &self,
        id: CaptureJobId,
        expected_epoch: i64,
        candidates: &[HostId],
    ) -> Result<Option<CaptureJobRow>, MetaError> {
        let _ = (id, expected_epoch, candidates);
        Err(MetaError::Migration(
            "capture jobs unsupported by this store".into(),
        ))
    }

    /// Re-drive a RETRYABLE TERMINAL (`failed`) job back onto a fresh
    /// host under the attempts budget — the automatic counterpart of the
    /// operator's `retry_enable_job` escape. [`Self::reassign_capture_job`]
    /// is fenced `stage NOT IN ('done', 'failed')`, so it can never move
    /// a row that already reached `failed`: the enable scanner's
    /// terminal-retryable arm would call it, match 0 rows, and silently
    /// loop on the same terminal row forever (the enable job never reaches
    /// `ready`/`failed`). This verb closes that hole with a fence that
    /// deliberately targets a terminal row: `UPDATE ... SET host_id =
    /// $new, epoch = epoch + 1, attempts = attempts + 1, stage =
    /// 'assigned', <reset per-attempt fields> WHERE id = $1 AND epoch =
    /// $2 AND stage = 'failed' AND retryable AND attempts < $4`.
    ///
    /// The `attempts < $max_attempts` clause makes the budget atomic: a
    /// row that has exhausted its attempts returns `None` (0 rows), and
    /// the caller fails the enable job with the terminal row's own failure
    /// kind, exactly as the non-retryable path does. The `epoch + 1` bump
    /// preserves the terminal-report-immutability property: any stale
    /// report from the just-abandoned attempt is fenced off by epoch, so
    /// [`Self::record_capture_job_report`] stays fenced on `(id, epoch)`
    /// and never needs weakening. Per-attempt fields
    /// (`error`/`error_stage`/`retryable`/`result_bincode`/
    /// `fc_snapshot_version`) are cleared and the stage/progress
    /// timestamps reset, mirroring what a fresh
    /// [`Self::insert_capture_job`] initializes.
    ///
    /// `None` when the fence missed: attempts exhausted, a racing
    /// coordinator replica already re-drove it (epoch moved), or the row
    /// is no longer a retryable `failed` — never silently ignore it, the
    /// caller must decide (fail the enable job, or observe the fresh
    /// attempt) rather than loop.
    async fn redrive_failed_capture_job(
        &self,
        id: CaptureJobId,
        expected_epoch: i64,
        candidates: &[HostId],
        max_attempts: u32,
    ) -> Result<Option<CaptureJobRow>, MetaError> {
        let _ = (id, expected_epoch, candidates, max_attempts);
        Err(MetaError::Migration(
            "capture jobs unsupported by this store".into(),
        ))
    }

    /// Every DISPATCHED (`host_id IS NOT NULL`) non-terminal job whose
    /// current stage has run longer than its budget in `budgets` (the
    /// deadline scan's read: `assigned` 60s, `booting` 300s absolute,
    /// `freezing` = `snapshot_create_timeout(mem_mib) + 60s`, etc. — the
    /// caller supplies the budgets since they depend on job-specific
    /// config like `mem_mib`). Deliberately EXCLUDES waiting jobs
    /// (`host_id IS NULL`): a waiting job has no stage deadline, only the
    /// queue timeout ([`Self::list_waiting_capture_jobs`]). Default:
    /// empty (a store without job support never has overdue jobs).
    async fn expire_capture_job_stages(
        &self,
        budgets: &[(CaptureJobStage, std::time::Duration)],
    ) -> Result<Vec<CaptureJobRow>, MetaError> {
        let _ = budgets;
        Ok(Vec::new())
    }

    /// Every WAITING (`host_id IS NULL`) non-terminal capture job — the
    /// queue-timeout scan's read. Each is re-offered to
    /// [`Self::place_capture_job`] every tick; one whose `waiting_since`
    /// is older than the queue timeout is failed with `CapacityTimeout`.
    /// Default: empty.
    async fn list_waiting_capture_jobs(&self) -> Result<Vec<CaptureJobRow>, MetaError> {
        Ok(Vec::new())
    }

    /// Active (non-terminal) job assignments currently on `host` — the
    /// `HeartbeatAck.capture_assignments` source, read every heartbeat
    /// regardless of whether any capture jobs exist fleet-wide.
    async fn capture_assignments_for_host(
        &self,
        host: HostId,
    ) -> Result<Vec<CaptureJobAssignment>, MetaError> {
        let _ = host;
        Ok(Vec::new())
    }

    /// The set of hosts with at least one active (non-terminal)
    /// capture job — placement's one-capture-per-host anti-affinity
    /// veto (`capture_jobs_active_host`).
    async fn hosts_with_live_capture_jobs(
        &self,
    ) -> Result<std::collections::HashSet<HostId>, MetaError> {
        Ok(std::collections::HashSet::new())
    }

    /// ADR 0084 P1b: mirror a [`CaptureJobReport`]'s stage/progress onto
    /// the owning `enable_jobs` row's ADR 0079 dashboard columns
    /// (`capture_phase`/`warm_stage`/`output_tail`) — UNFENCED (no
    /// `claimed_by` check, no claim renewal): `capture_jobs` is now
    /// authoritative for execution and fencing; this is cosmetic
    /// dashboard mirroring only, driven by the heartbeat reconcile on
    /// every report regardless of which coordinator pod (if any)
    /// currently holds the enable job's claim. `None` leaves a column
    /// unchanged (`COALESCE`) — a report with no rendered phase
    /// (`assigned`/`done`/`failed`) shouldn't blank the last-known
    /// warm-hook stage/output an operator was reading.
    async fn mirror_capture_progress_to_enable_job(
        &self,
        enable_job_id: uuid::Uuid,
        capture_phase: Option<&str>,
        warm_stage: Option<&str>,
        output_tail: Option<&str>,
        // ADR 0088 addendum: the capture timeline (JSON array of
        // `WarmStageRecord`) for `enable_jobs.warm_stages`; `None`
        // keeps the last-known timeline (COALESCE, like the rest).
        warm_stages: Option<&serde_json::Value>,
    ) -> Result<(), MetaError> {
        let _ = (
            enable_job_id,
            capture_phase,
            warm_stage,
            output_tail,
            warm_stages,
        );
        Ok(())
    }

    /// Stamp the terminal `reuse_outcome` on an enable job (ADR 0084
    /// section D): `reused_full | reused_cold_base |
    /// recaptured:no_cold_base | recaptured:content_changed |
    /// recaptured:chunks_missing | recaptured:fc_version_changed`. A
    /// plain write, not fenced by claimant — it's stamped once the job
    /// has already reached a terminal enable-job state.
    async fn set_enable_job_reuse_outcome(
        &self,
        enable_job_id: uuid::Uuid,
        outcome: &str,
    ) -> Result<(), MetaError> {
        let _ = (enable_job_id, outcome);
        Err(MetaError::Migration(
            "capture jobs unsupported by this store".into(),
        ))
    }

    /// Insert or replace the cold base at `row.content_key` (ADR 0084
    /// section B) — the content-keyed, boot-to-agentd-ready Full
    /// snapshot every warm-image capture of matching content reuses
    /// (the hook always re-runs against a fresh env on top of it).
    async fn upsert_cold_base(&self, row: ColdBaseRow) -> Result<(), MetaError> {
        let _ = row;
        Err(MetaError::Migration(
            "capture jobs unsupported by this store".into(),
        ))
    }

    /// Look up a cold base by its content key — the reuse-candidate
    /// read the executor's miss/hit decision drives off of. Default
    /// `None`, mirroring [`Self::find_enabled_image_by_content`]: a
    /// store without the query surface simply never reuses.
    async fn get_cold_base(&self, content_key: &str) -> Result<Option<ColdBaseRow>, MetaError> {
        let _ = content_key;
        Ok(None)
    }

    /// Every cold base's `snapshot_id` — joins the ADR 0077 GC pin-set
    /// roots (mirrors [`Self::bundle_pin_set`]/
    /// [`Self::snapshot_blob_pin_set`]'s always-called-unconditionally
    /// shape). Default: empty.
    async fn cold_base_snapshot_ids(&self) -> Result<Vec<SnapshotId>, MetaError> {
        Ok(Vec::new())
    }

    /// Every cold base's `disk_manifest` + `memory_manifest`, parsed —
    /// the chunk-GC pin-set's 7th source (`PinSet::collect`, ADR 0084
    /// §B6): a cold base's chunks have no OTHER root (unlike the
    /// overlay snapshot it seeds, it never gets its own `snapshots` row
    /// pinned via `snapshot_blob_pin_set`/`enabled_images`), so without
    /// this they'd be silently reaped out from under a live
    /// `cold_bases` row. Default: empty (no second GC — a store without
    /// `cold_bases` support has nothing to pin).
    async fn cold_base_manifest_refs(
        &self,
    ) -> Result<Vec<crate::types::manifest::ManifestRef>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0084 §D: does a `cold_bases` row exist for `disk_manifest`
    /// under a DIFFERENT `fc_snapshot_version` than `current_fc_version`?
    /// Powers the `recaptured:fc_version_changed` reuse-outcome label —
    /// distinguishing "this rootfs was captured before, just under an
    /// older/newer FC build" from a genuine first-time
    /// `recaptured:no_cold_base`. Matches on `disk_manifest` alone (not
    /// the full content key, which already bakes in the version and so
    /// can never itself answer "under a DIFFERENT version") — `cold_bases`
    /// has no `resources` column to refine further, and every row in the
    /// table is FC's by construction (VZ/Process never write one), so
    /// this is a sound approximation for a telemetry label, not a
    /// correctness gate. Default `false` (a store without this query
    /// surface just reports the coarser `no_cold_base` label instead).
    async fn cold_base_fc_version_changed(
        &self,
        disk_manifest: &str,
        current_fc_version: &str,
    ) -> Result<bool, MetaError> {
        let _ = (disk_manifest, current_fc_version);
        Ok(false)
    }

    // ---- session secrets ----
    //
    // Per-request `secrets` overrides supplied at session-create,
    // sealed under the deployment KEK. The resume path opens the
    // sealed blob to rebuild the post-resume harness's launch env;
    // without persistence the in-VM bootstrap respawns a Claude
    // child with no OAuth token and the user gets re-prompted to
    // log in. The write happens inside `reserve_and_persist_create`'s
    // transaction (`SessionCreateWriteSet::sealed_secrets`) — there is no
    // standalone upsert; only `get`/`delete` remain as trait methods.
    async fn get_session_secrets(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionSecrets>, MetaError>;
    async fn delete_session_secrets(&self, session_id: SessionId) -> Result<(), MetaError>;

    // ---- session capabilities (ADR 0056) ----
    //
    // The `(provider, action, resource)` grants a profile declared, bound
    // to the session at create (`session_capabilities`). The broker reads
    // them to clamp every guest credential/action request. Default impls so
    // stores that don't model capabilities (test doubles) compile unchanged;
    // `PostgresStore` is the authority. `bind` is idempotent and a no-op on
    // an empty set — both create paths (boot + enqueue) call it, and the
    // queued-then-booted re-prepare carries an empty set (the rows were
    // bound at enqueue), so the no-op preserves them.
    async fn bind_session_capabilities(
        &self,
        session_id: SessionId,
        caps: &[Capability],
    ) -> Result<(), MetaError> {
        let _ = (session_id, caps);
        Ok(())
    }
    async fn get_session_capabilities(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<Capability>, MetaError> {
        let _ = session_id;
        Ok(Vec::new())
    }

    // ---- session integration policy (ADR 0056 B′) ----
    //
    // The orchestrator-compiled per-session policy, persisted verbatim as its
    // JSON string so the queued re-prepare + resume can rebuild the egress
    // injects without the orchestrator. One blob per session (upsert). Default
    // impls (no-op / None) so test doubles compile; `PostgresStore` is the
    // authority.
    async fn bind_session_integration_policy(
        &self,
        session_id: SessionId,
        policy_json: &str,
    ) -> Result<(), MetaError> {
        let _ = (session_id, policy_json);
        Ok(())
    }
    async fn get_session_integration_policy(
        &self,
        session_id: SessionId,
    ) -> Result<Option<String>, MetaError> {
        let _ = session_id;
        Ok(None)
    }

    /// ADR 0062: the session's persisted harness selection, or `None`. The
    /// write happens inside `reserve_and_persist_create`'s transaction
    /// (issue #535 (b)) — there is no standalone setter.
    async fn get_session_harness(
        &self,
        session_id: SessionId,
    ) -> Result<Option<String>, MetaError> {
        let _ = session_id;
        Ok(None)
    }

    /// ADR 0016 Phase B: write the host's freshly-flushed disk
    /// manifest into `sessions.live_disk_manifest_*`, gated by a
    /// `sandbox_id` match so a stale publish from a destroyed
    /// sandbox can't clobber a fresh binding. Bumps
    /// `chunk_generation` in the same transaction on `Applied` so
    /// Phase C's GC barrier sees an atomic step from "old pin set
    /// → write → new pin set" (no mid-sweep window where the new
    /// manifest exists but the generation hasn't ticked).
    ///
    /// `DroppedStale` means the UPDATE matched zero rows — either
    /// the session was destroyed between flush and publish, or the
    /// sandbox was rebound. Coord-side handler logs a `warn!` with
    /// the mismatch and returns 200 to the host (the host shouldn't
    /// retry; the staleness is structural).
    ///
    /// Default returns `Applied` and does nothing, so test mocks
    /// don't need to plumb the columns until they exercise Phase B.
    async fn update_live_disk_manifest(
        &self,
        _session_id: SessionId,
        _sandbox_id: SandboxId,
        _manifest_ref: ManifestRef,
    ) -> Result<UpdateOutcome, MetaError> {
        Ok(UpdateOutcome::Applied)
    }

    /// ADR 0016 Phase C support: read the one-row `chunk_generation`
    /// counter. Phase C's GC sweep reads this before listing the
    /// pin set and again at the end; if it ticked, the sweep
    /// restarts with a fresh pin set. Default `0` so non-PG
    /// backends don't have to track it.
    async fn chunk_generation(&self) -> Result<u64, MetaError> {
        Ok(0)
    }

    /// ADR 0016 Phase C: bump `chunk_generation` independent of a
    /// flush. Used by `enable_image` and `record_snapshot` writes
    /// (commit 2 of the Phase C chain) to keep the barrier complete
    /// across all manifest-lineage-producing paths, not just live
    /// disk flushes. Default is a no-op so non-PG backends opt out
    /// cleanly. PG override increments the single row.
    async fn bump_chunk_generation(&self) -> Result<(), MetaError> {
        Ok(())
    }

    /// ADR 0016 Phase C: enumerate every chunked-disk `ManifestRef`
    /// currently advertised by an `enabled_images` row. Pin-set
    /// source #1: every enabled chunked image pins its base
    /// manifest's chunks. Harness-only images (NULL `disk_manifest_*`
    /// columns) drop out of the query naturally via the partial
    /// index from migration 0036.
    ///
    /// Default `Ok(vec![])` keeps in-memory mocks quiet; PG impl
    /// returns DISTINCT (id, version) tuples wrapped as `ManifestRef`.
    async fn list_enabled_image_disk_manifest_ids(&self) -> Result<Vec<ManifestRef>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0022 Option A: pin-set source #5 — every enabled image's
    /// **base-snapshot memory manifest** (the per-template base memfile's
    /// backing chunks). The memfile is a shared, immutable artifact that
    /// every same-template base `session.create` sibling `MAP_PRIVATE`s;
    /// its chunks must stay pinned for the template's *enabled* lifetime,
    /// **independent of the base snapshot row's `recoverable` flag** —
    /// exactly as source #1 pins the rootfs disk manifest independent of
    /// any snapshot. Reads the existing `enabled_images
    /// .base_snapshot_memory_manifest_*` columns (migrations 0043/0049);
    /// `NULL` for cold-boot backends (VZ) drops out via `IS NOT NULL`. No
    /// `soft_deleted_at` filter (mirrors source #1, ADR 0021 P1.8): a
    /// disabled-but-present image with live sharers keeps its memfile
    /// pinned. Default `Ok(vec![])` for non-PG mocks.
    async fn list_enabled_image_base_snapshot_memory_manifests(
        &self,
    ) -> Result<Vec<ManifestRef>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0022 Option A: pin-set source #6 — the disk companion to #5.
    /// Pins every enabled image's **base-snapshot disk manifest** (the
    /// rootfs a base `session.create` restores from, migration 0042),
    /// again independent of the base snapshot row's `recoverable` flag, so
    /// an enabled template is fully self-pinned (memory + disk) without
    /// relying on the snapshot row's state. Reads
    /// `enabled_images.base_snapshot_disk_manifest_*`. Default
    /// `Ok(vec![])` for non-PG mocks.
    async fn list_enabled_image_base_snapshot_disk_manifests(
        &self,
    ) -> Result<Vec<ManifestRef>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0016 Phase C: enumerate every `(live_disk_manifest_id,
    /// live_disk_manifest_version)` pair currently advertised by a
    /// session row. Pin-set source #2 (the other two are
    /// `list_enabled_image_disk_manifest_ids` and
    /// `list_live_disk_manifest_ids` / `list_live_memory_manifest_ids`
    /// over snapshots).
    ///
    /// Returns DISTINCT (id, version) tuples wrapped as `ManifestRef`.
    /// Default `Ok(vec![])` keeps in-memory mocks quiet; PG impl
    /// runs an indexed `WHERE live_disk_manifest_id IS NOT NULL`
    /// query against the partial index added by migration 0034.
    async fn list_live_session_disk_manifest_ids(&self) -> Result<Vec<ManifestRef>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0016 Phase C: upsert a chunk into the GC candidate table.
    /// Idempotent under `ON CONFLICT (content_hash) DO UPDATE SET
    /// last_seen_at = now()` so re-seeing a candidate refreshes the
    /// diagnostic timestamp without resetting `first_seen_at` (the
    /// 24h grace window must not be extended by repeated sightings).
    ///
    /// `hash` is the raw sha256 digest (`ChunkHash::as_bytes()`
    /// from `engram-chunk-store`). Trait stays primitive at the
    /// boundary to avoid an `engram-core` → `engram-chunk-store`
    /// dep cycle.
    async fn upsert_chunk_gc_candidate(&self, _hash: [u8; 32]) -> Result<(), MetaError> {
        Ok(())
    }

    /// ADR 0016 Phase C promote-pass query. Returns up to `limit`
    /// candidate hashes whose `first_seen_at` predates `cutoff`,
    /// ordered by `first_seen_at` so the oldest backlog drains
    /// first. The caller deletes each from BlobStorage and then
    /// passes the same hashes to [`delete_gc_candidates`].
    ///
    /// Batched (`limit` is required, not optional) so a single
    /// sweep with a large backlog can't OOM the coord pod.
    async fn list_expired_gc_candidates(
        &self,
        _cutoff: chrono::DateTime<chrono::Utc>,
        _limit: i64,
    ) -> Result<Vec<[u8; 32]>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0016 Phase C: batch-delete candidate rows after the
    /// BlobStorage delete succeeded. Order doesn't matter; missing
    /// rows are silently skipped (idempotent so a re-run on a
    /// partial promote pass is safe).
    async fn delete_gc_candidates(&self, _hashes: &[[u8; 32]]) -> Result<(), MetaError> {
        Ok(())
    }

    /// ADR 0016 Phase C: paged read of the candidate table for the
    /// `GET /api/admin/chunk-gc/candidates` operator surface. When
    /// `before` is `Some`, filters to rows where `first_seen_at <
    /// before` (matches the promote-pass shape but exposed for
    /// inspection). When `None`, returns everything up to `limit`.
    /// Ordered by `first_seen_at` ascending so the oldest backlog
    /// comes first.
    async fn list_gc_candidates(
        &self,
        _limit: i64,
        _before: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<GcCandidateRow>, MetaError> {
        Ok(Vec::new())
    }

    // ----------------------------------------------------------------
    // ADR 0035 — bundle-generation GC (mirrors the chunk GC trio).

    /// ADR 0035 §5: the bundle pin set — every `(drive_id, sha256)`
    /// some `snapshots.aux_bundles` row references, **∪ every live
    /// `mount_catalog` skill** (ADR 0055 P2, so a registered-but-currently-unused
    /// uploaded skill stays staged + survives GC) **∪ every live `harness_catalog`
    /// row** (ADR 0062, so a registered-but-currently-unused *custom* harness's
    /// squashfs stays staged — built-ins ride the host-image stamp and need no
    /// pin). The union (plus the hosts' reported current generations) is what the
    /// GC keeps and what heartbeat acks advertise as `live_bundles`.
    async fn bundle_pin_set(&self) -> Result<Vec<crate::types::sandbox::AuxBundleRef>, MetaError> {
        Ok(Vec::new())
    }

    // ----------------------------------------------------------------
    // ADR 0055 P2 — org-shared user-uploaded skill catalog (`mount_catalog`).

    /// Register (upsert by `name`) a packed, content-addressed user-uploaded
    /// skill. Idempotent by content: re-registering identical bytes yields the
    /// same `sha256`. `owner` is attribution only — the catalog is org-shared.
    /// Returns the live row. A name matching an existing catalog row updates it
    /// (new sha/owner/description); collision with a *fleet* bundle name is
    /// rejected by the caller before this is reached.
    async fn register_skill(
        &self,
        _owner: &str,
        _name: &str,
        _description: &str,
        _sha256: &str,
        _mount_json: &str,
        _size_bytes: i64,
    ) -> Result<crate::types::CatalogSkill, MetaError> {
        Err(MetaError::Db(
            "register_skill is not supported by this MetadataStore".into(),
        ))
    }

    /// Every live catalog skill (newest first) — for the orchestrator's catalog
    /// listing + profile-editor validation.
    async fn list_skills(&self) -> Result<Vec<crate::types::CatalogSkill>, MetaError> {
        Ok(Vec::new())
    }

    /// Resolve one selected skill name to its live catalog row, if any. The
    /// session-create resolver calls this for a name absent from the fleet
    /// stamp (ADR 0055 P2: `fleet_stamp ∪ mount_catalog`).
    async fn get_skill_by_name(
        &self,
        _name: &str,
    ) -> Result<Option<crate::types::CatalogSkill>, MetaError> {
        Ok(None)
    }

    /// Soft-delete a catalog skill by name (sets `deleted_at`). Dropping it from
    /// the pin set lets the existing bundle GC reclaim its blob after the grace
    /// window (upload-path GC). Returns whether a live row was deleted.
    async fn soft_delete_skill(&self, _name: &str) -> Result<bool, MetaError> {
        Ok(false)
    }

    // ----------------------------------------------------------------
    // ADR 0062 — the harness catalog (`harness_catalog`).

    /// Register (upsert by `name`) a harness into the catalog. `owner` is
    /// attribution only — the catalog is org-shared (like skills). Returns the
    /// live row.
    async fn register_harness(
        &self,
        _reg: crate::types::HarnessRegistration<'_>,
    ) -> Result<crate::types::CatalogHarness, MetaError> {
        Err(MetaError::Db(
            "register_harness is not supported by this MetadataStore".into(),
        ))
    }

    /// Every live catalog harness (newest first) — for the orchestrator's
    /// `ListHarnesses` listing + profile-editor validation.
    async fn list_harnesses(&self) -> Result<Vec<crate::types::CatalogHarness>, MetaError> {
        Ok(Vec::new())
    }

    /// Resolve one harness name to its live catalog row, if any.
    async fn get_harness_by_name(
        &self,
        _name: &str,
    ) -> Result<Option<crate::types::CatalogHarness>, MetaError> {
        Ok(None)
    }

    /// Soft-delete a catalog harness by name (sets `deleted_at`). Returns
    /// whether a live row was deleted.
    async fn soft_delete_harness(&self, _name: &str) -> Result<bool, MetaError> {
        Ok(false)
    }

    /// ADR 0035 §5: idempotent candidate upsert; `first_seen_at`
    /// sticky, same grace semantics as the chunk variant. `sha256`
    /// is the 64-hex digest (text — bundle counts are tiny).
    async fn upsert_bundle_gc_candidate(&self, _sha256: &str) -> Result<(), MetaError> {
        Ok(())
    }

    /// ADR 0035 §5 promote-pass query (oldest first, batched).
    async fn list_expired_bundle_gc_candidates(
        &self,
        _cutoff: chrono::DateTime<chrono::Utc>,
        _limit: i64,
    ) -> Result<Vec<String>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0035 §5: batch-delete candidate rows after the blob
    /// delete succeeded. Idempotent.
    async fn delete_bundle_gc_candidates(&self, _sha256s: &[String]) -> Result<(), MetaError> {
        Ok(())
    }

    // ----------------------------------------------------------------
    // ADR 0028 addendum — portable snapshot-blob GC (mirrors the chunk
    // + bundle GC trios). Governs `snapshots/<id>/{state.bin,sidecar,
    // ...}` the same way: pinned by a live `snapshots` row, swept when
    // unreferenced after the shared grace period. Replaces the host's
    // inline `abort_prior_inflight_snapshot` blob deletion, which could
    // delete a recorded `recoverable=true` snapshot out from under a
    // resume.

    /// The snapshot-blob pin set: every snapshot id with a live row.
    /// LOAD-BEARING — this MUST be every row (`SELECT id FROM
    /// snapshots`), with NO `recoverable` filter (the resume self-heal
    /// transiently demotes rows) and NO `session_id` filter
    /// (`session_id IS NULL` template/base captures are referenced by
    /// `enabled_images.base_snapshot_id` and have no self-heal
    /// backstop). The set is exact — a blob is pinned IFF its row
    /// exists. A *current* base row is never reaped
    /// (`prune_orphan_base_snapshots` skips ids still referenced by
    /// `enabled_images.base_snapshot_id`, and the `base_snapshot_id` FK
    /// with no `ON DELETE` is a hard backstop), so its blobs stay
    /// pinned; a *superseded* base that reaper deletes correctly drops
    /// out of the pin set so its now-unreferenced blobs get collected.
    /// `prune_session_snapshots` (`session_id IS NOT NULL`) never
    /// touches base rows.
    async fn snapshot_blob_pin_set(&self) -> Result<Vec<crate::types::SnapshotId>, MetaError> {
        Ok(Vec::new())
    }

    /// Idempotent candidate upsert; `first_seen_at` sticky, same grace
    /// semantics as the chunk + bundle variants.
    async fn upsert_snapshot_blob_gc_candidate(
        &self,
        _id: crate::types::SnapshotId,
    ) -> Result<(), MetaError> {
        Ok(())
    }

    /// Promote-pass query (oldest first, batched).
    async fn list_expired_snapshot_blob_gc_candidates(
        &self,
        _cutoff: chrono::DateTime<chrono::Utc>,
        _limit: i64,
    ) -> Result<Vec<crate::types::SnapshotId>, MetaError> {
        Ok(Vec::new())
    }

    /// Batch-delete candidate rows after the blob delete succeeded.
    /// Idempotent.
    async fn delete_snapshot_blob_gc_candidates(
        &self,
        _ids: &[crate::types::SnapshotId],
    ) -> Result<(), MetaError> {
        Ok(())
    }

    // ----------------------------------------------------------------
    // ADR 0018 commit 12b — evac_resumer scanner support.
    //
    // The scanner polls `Evacuating` sessions, picks a peer host,
    // and drives `Evacuating → Created → Active`. The retry counter
    // is a side-car on the `sessions` row (column `evac_attempts`,
    // migration 0037). The PG-backed `transition_session(Evacuating)`
    // resets the counter to 0 in the same UPDATE so re-entry from a
    // fresh drain starts fresh; bumps happen via `bump_evac_attempts`
    // (atomic UPDATE ... RETURNING).
    // ----------------------------------------------------------------

    /// Sessions currently in `Evacuating`, paired with their current
    /// `evac_attempts` count. The scanner uses this on every tick.
    /// Default `Ok(vec![])` keeps in-memory mocks quiet; PG impl
    /// runs an indexed `WHERE status = 'evacuating'` query.
    async fn list_evacuating_sessions(&self) -> Result<Vec<(Session, u32)>, MetaError> {
        Ok(Vec::new())
    }

    /// Atomically `evac_attempts = evac_attempts + 1 RETURNING
    /// evac_attempts`. Scanner calls this before each resume attempt;
    /// when the returned count exceeds the budget, scanner gives up
    /// and falls back to Idle. Default returns 1 so test mocks can
    /// observe the bump without persisting state.
    async fn bump_evac_attempts(&self, _session_id: SessionId) -> Result<u32, MetaError> {
        Ok(1)
    }

    // ----------------------------------------------------------------
    // ADR 0034 — eviction scanner + idle-detection backstop support.
    //
    // Mirrors the 12b evac shape above: `Evicting` rows carry a
    // side-car retry counter (column `evict_attempts`, migration
    // 0050) that `transition_session(Evicting)` resets in the same
    // UPDATE, and the coord-side eviction scanner sweeps the state on
    // a 10s tick. The backstop query is the L3 detector: it asks PG —
    // not the host's in-memory hub — which Active sessions have gone
    // silent, catching harness-detach / host-amnesia classes the hub
    // structurally cannot see.
    // ----------------------------------------------------------------

    /// Sessions currently in `Evicting`, paired with their current
    /// `evict_attempts` count. The eviction scanner uses this on
    /// every tick (and on its first tick after coord startup, which
    /// is what recovers rows wedged across a deploy). Default
    /// `Ok(vec![])` keeps in-memory mocks quiet; PG impl runs the
    /// partial-indexed `WHERE status = 'evicting'` query.
    async fn list_evicting_sessions(&self) -> Result<Vec<(Session, u32)>, MetaError> {
        Ok(Vec::new())
    }

    /// Atomically `evict_attempts = evict_attempts + 1 RETURNING
    /// evict_attempts`. The eviction scanner calls this before each
    /// pipeline attempt; past the budget it falls back to HostLost
    /// (see ADR 0034 for why not Active/Idle/Dead). Default returns 1
    /// so test mocks can observe the bump without persisting state.
    async fn bump_evict_attempts(&self, _session_id: SessionId) -> Result<u32, MetaError> {
        Ok(1)
    }

    /// ADR 0034 L3 backstop: `Active` sessions with a bound sandbox
    /// whose newest `session_events` row is older than
    /// `idle_for_secs` (falling back to the session's `created_at`
    /// when no events exist yet). These are sessions the host-side
    /// idle detector has gone blind to — harness detached, host-agent
    /// restarted, hub bookkeeping lost — and they would otherwise sit
    /// Active forever. Returns `(session_id, sandbox_id,
    /// last_event_at)`; the caller nominates them into the Evicting
    /// lane. Default empty for non-PG mocks.
    async fn list_active_sessions_idle_past(
        &self,
        _idle_for_secs: i64,
    ) -> Result<Vec<(SessionId, SandboxId, chrono::DateTime<chrono::Utc>)>, MetaError> {
        Ok(Vec::new())
    }

    /// ADR 0029: fleet-wide snapshot totals for the Storage surface —
    /// the count of `snapshots` rows and the sum of their
    /// `size_bytes`. A single cheap aggregate query (the Storage page
    /// polls on a slow cadence). Default zeros so non-PG mocks stay
    /// quiet; the PG impl runs `SELECT count(*), coalesce(sum(...),0)`.
    async fn snapshot_totals(&self) -> Result<SnapshotTotals, MetaError> {
        Ok(SnapshotTotals::default())
    }

    /// ADR 0029: the number of chunks currently parked in
    /// `chunk_gc_candidates` awaiting their grace window — the
    /// "gc pending" rollup on the Storage surface. A cheap
    /// `SELECT count(*)`; default `0` for non-PG mocks.
    async fn count_gc_candidates(&self) -> Result<u64, MetaError> {
        Ok(0)
    }
}

/// Fleet-wide snapshot aggregate from [`MetadataStore::snapshot_totals`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SnapshotTotals {
    pub count: u64,
    pub total_bytes: u64,
}

/// One row from [`MetadataStore::list_gc_candidates`]. Surfaced
/// verbatim through the admin `GET /chunk-gc/candidates` endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcCandidateRow {
    pub content_hash: [u8; 32],
    pub first_seen_at: chrono::DateTime<chrono::Utc>,
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
}

/// Outcome of [`MetadataStore::update_live_disk_manifest`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// The session row was updated and `chunk_generation` ticked.
    /// Coord handler logs `info!`; host's coalescing mpsc moves on.
    Applied,
    /// The UPDATE matched zero rows — `sessions.sandbox_id !=
    /// publish.sandbox_id` (destroyed/rebound) or the session is
    /// gone. Coord handler logs `warn!` with the mismatch fields;
    /// host shouldn't retry.
    DroppedStale,
}
