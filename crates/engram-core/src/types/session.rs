use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

use super::ids::{HostId, SandboxId, SessionId};

/// ADR 0015 M2: explicit session lifecycle state machine.
///
/// Each variant has a published meaning; transitions are validated
/// against [`SessionState::can_transition_to`] / [`try_transition_to`]
/// so illegal moves surface as runtime errors instead of silently
/// corrupting the row. The legality table is the single source of
/// truth — call sites do not encode their own preconditions.
///
/// Persistence: the column is `TEXT`; the wire form is the
/// snake_case spelling of each variant. (`Pending` is persisted since
/// ADR 0046 — a sandbox-less `pending` row is the placement
/// reservation — and is also the `from` of the first `StatusChanged`
/// event a freshly-created session emits.)
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Request accepted; a host has been reserved (ADR 0046 — a
    /// sandbox-less `pending` row IS the reservation) but the sandbox
    /// isn't bound yet. Also the `from` side of the first
    /// `StatusChanged` event a freshly-created session emits.
    Pending,
    /// ADR 0048: accepted but no host had capacity, so instead of a 503
    /// the session waits in a FIFO queue (`host_id` NULL, `queued_at`
    /// set). The queue scanner re-attempts placement each tick and
    /// either drives it forward (`Queued → Pending` for a create,
    /// `Queued → Idle` to resume) or fails it on timeout
    /// (`Queued → Failed`). Holds no host memory reservation.
    Queued,
    /// Sandbox is bound to a host (`host_id` + `sandbox_id` populated)
    /// but nothing further is proven. `start_agent` has not yet run;
    /// agentd may not be reachable; harness (if any) has not been
    /// spawned. Calls into `/exec`, `/shell`, `/prompt` against a
    /// `Created` session return 409.
    Created,
    /// Harness is running (or `harness=none` and agentd is ready). The
    /// only state in which `/exec`, `/shell`, `/prompt` proceed
    /// without a state-mismatch error.
    Active,
    /// ADR 0091: the coordinator believes a sandbox exists on a live,
    /// heartbeating host, but the GUEST is not responding on the
    /// control plane (the host's periodic-checkpoint driver hit N
    /// consecutive socket-level failures against the FC API socket).
    /// Distinct from `HostLost` (the whole host vanished): the host is
    /// fine, the VM is dead/wedged. Non-terminal — a prompt/exec
    /// auto-drives the existing checkpoint recovery (relocating if
    /// needed), and a healed probe flips back to `Active`. Pre-ADR, a
    /// dead guest read `active` indefinitely with every exec bouncing.
    Unreachable,
    /// Sandbox has been evicted to a snapshot in BlobStorage. Resumes
    /// via `Idle → Created → Active` (the resume path re-runs the
    /// create-shape transitions on the new sandbox).
    Idle,
    /// Heartbeat-loss against the bound host. Non-terminal: the
    /// dead-host detector's second stage moves it onward —
    /// `HostLost → Idle` when the session is recoverable (a snapshot
    /// OR a live disk manifest exists; the user/exec `/resume`s on
    /// next access, ADR 0045 Phase A), or `HostLost → Dead` when
    /// there is nothing to recover. The detector no longer routes
    /// into `Evacuating` (the reactive auto-evac is retired).
    HostLost,
    /// ADR 0018 commit 12: session is mid-relocation. The source host
    /// has paused FC, flushed dirty pages, captured a memory snapshot,
    /// destroyed the local sandbox, and durably committed both memory
    /// and disk manifests to BlobStorage. The coord-side
    /// `evac_resumer` background task scans for sessions in this
    /// state and drives `Evacuating → Created → Active` on a peer
    /// host via the same `resume_session` machinery `/resume from
    /// Idle` uses. After 20 failed peer-pick / restore attempts
    /// (~3 min), falls back to `Idle` so a user `/resume` can drive
    /// it forward by hand. Reached **only** from `Active` via operator
    /// drain (`POST /api/admin/sessions/:id/evacuate` or
    /// `POST /api/admin/hosts/:id/drain`) — ADR 0044 K3. As of ADR
    /// 0045 Phase A the dead-host detector no longer routes here (the
    /// reactive auto-evac is retired), and `HostLost → Evacuating` is
    /// no longer a legal edge. Sandbox + host bindings are nulled out
    /// same as `Idle` — the session is recoverable but not running.
    Evacuating,
    /// ADR 0034: durable idle-eviction intent marker. The candidates
    /// handler (or the PG detection backstop) transitions
    /// `Active → Evicting` and returns immediately; the coord-side
    /// evict op's pipeline (ADR 0079, `run_evict_pipeline`) drives it
    /// to its terminal `Evicting → Idle`. A *pre-pipeline* marker, not
    /// the pipeline's target: the pipeline's internals (op claim, K5
    /// guard, abort-on-failure) are unchanged, and a coord restart
    /// mid-eviction leaves an op row the executor's reclaim sweep (or
    /// the scanner's re-enqueue) picks up. Unlike `Idle`/`Evacuating`, the sandbox is
    /// (usually) still RUNNING — `sandbox_id` stays bound until the
    /// pipeline nulls it, and `/exec`/`/prompt`/`/resume` return a
    /// retryable 409 rather than auto-resuming. After 20 failed
    /// pipeline attempts (~3 min) falls back to `HostLost` (honest:
    /// "coord can't reconcile this runtime"; loop-free — see the ADR
    /// for why Active/Idle/Dead are each wrong).
    Evicting,
    /// Terminal: create failed mid-flight (insert failed,
    /// `start_agent` failed, or scheduling collapsed after row
    /// insertion).
    Failed,
    /// Terminal: user-initiated delete.
    Completed,
    /// Terminal: the session cannot resume. Engram is a one-shot task
    /// runner; `Dead` ends the session. A session reaches `Dead` when
    /// its chunked manifests are unreferenceable (ADR 0007 GC, never
    /// written, chunk store lost them) or when the user explicitly
    /// destroys. Callers either accept the loss or start a fresh
    /// session.
    Dead,
}

impl SessionState {
    /// ADR 0046: states in which the session has a live VM resident on its host
    /// and therefore holds a memory reservation. Excludes `Idle` (sandbox torn
    /// down to a snapshot), `HostLost` (host gone), and the terminal states. The
    /// reservation is held across the *entire* eviction — the snapshot can take
    /// minutes (#147) and the VM is resident the whole time — and released only
    /// at teardown to `Idle`/terminal. A paused VM keeps status `Active`, so it
    /// stays reserved.
    pub fn reserves_host_memory(&self) -> bool {
        matches!(
            self,
            Self::Pending
                | Self::Created
                | Self::Active
                | Self::Unreachable
                | Self::Evacuating
                | Self::Evicting
        )
    }

    /// SQL-literal twin of [`Self::reserves_host_memory`], for the
    /// `status IN (…)` reservation aggregate. Kept in lockstep with the matcher
    /// by `reserving_states_match` (test).
    pub const fn host_memory_reserving_states() -> &'static [&'static str] {
        &[
            "pending",
            "created",
            "active",
            "unreachable",
            "evacuating",
            "evicting",
        ]
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Queued => "queued",
            Self::Created => "created",
            Self::Active => "active",
            Self::Unreachable => "unreachable",
            Self::Idle => "idle",
            Self::HostLost => "host_lost",
            Self::Evacuating => "evacuating",
            Self::Evicting => "evicting",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Dead => "dead",
        }
    }

    /// Terminal states reject every outgoing transition. Used by the
    /// legality table; exposed so callers can short-circuit cheaply
    /// (e.g. skip enqueuing work for a `Completed` session).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Failed | Self::Completed | Self::Dead)
    }

    /// "Live" = what `MetadataStore::list_active_sessions` returns:
    /// every non-terminal state except `HostLost` (limbo pending the
    /// reconciler; its bindings are stale by definition). Must stay
    /// in sync with the `status IN (...)` list in the Postgres impl —
    /// in particular every state that can carry a live `sandbox_id`
    /// binding (`Evicting` included) must be live, or the coord's
    /// startup `repopulate_routing` strands the session after a pod
    /// roll. Mock stores filter with this so they can't drift.
    pub fn is_live(&self) -> bool {
        // `Queued` is excluded alongside `HostLost`: it has no sandbox
        // binding to rehydrate (host_id NULL), and the queue scanner
        // finds it via its own FIFO query, not `list_active_sessions`.
        !self.is_terminal() && !matches!(self, Self::HostLost | Self::Queued)
    }

    /// Single source of truth for legal transitions. Per ADR 0015 M2
    /// + ADR 0018 commit 12 (Evacuating) + ADR 0034 (Evicting):
    ///
    /// ```text
    /// Pending     -> Created | Failed | Queued
    /// Queued      -> Pending (placed create) | Idle (resume dequeue
    ///                / resume-origin timeout) | Failed (create timeout
    ///                / cancel)
    /// Created     -> Active | Failed | HostLost
    /// Active      -> Idle | HostLost | Evacuating | Evicting | Failed
    ///              | Completed | Dead
    /// Idle        -> Created (resume) | Dead | Completed | Queued (resume
    ///                hit no capacity, ADR 0048)
    /// HostLost    -> Created | Idle | Dead | Completed
    /// Evacuating  -> Created (scanner resumes on peer)
    ///              | Idle (scanner exhausted retries; user /resume)
    ///              | Dead (terminal; chunks gone)
    ///              | Completed (user delete mid-evac)
    /// Evicting    -> Active (ADR 0074 rung-1 cancel: the user came back
    ///                 before capture began; lease-guarded)
    /// Evicting    -> Idle (eviction pipeline success)
    ///              | HostLost (scanner exhausted retries; host died
    ///                mid-eviction via the dead-host sweep)
    ///              | Dead (chunks unreferenceable)
    ///              | Completed (user delete mid-eviction)
    /// Failed      -> (terminal)
    /// Completed   -> (terminal)
    /// Dead        -> (terminal)
    /// ```
    ///
    /// Self-transitions (e.g. `Active → Active`) are illegal: every
    /// state change must be observable by event subscribers, and a
    /// no-op transition is almost certainly a logic bug (concurrent
    /// callers racing on the same UPDATE, double-fired event).
    pub const fn can_transition_to(&self, target: Self) -> bool {
        use SessionState::*;
        // One arm per `from` state — keeps the table readable when
        // adding/removing edges (and stays in sync with the doc-
        // comment block above). Every `from` state covered, including
        // the three terminal arms that always return false.
        match self {
            Pending => matches!(target, Created | Failed | Queued),
            // ADR 0048: a placed create flips Queued → Pending (re-enters
            // the normal boot path); a resume dequeue or a resume-origin
            // timeout goes Queued → Idle; a create-origin timeout / cancel
            // goes Queued → Failed.
            Queued => matches!(target, Pending | Idle | Failed),
            Created => matches!(target, Active | Failed | HostLost),
            Active => matches!(
                target,
                Idle | HostLost | Evacuating | Evicting | Failed | Completed | Dead | Unreachable
            ),
            // ADR 0091: Unreachable reaches everything Active can (the
            // recovery machinery is unchanged) plus back to Active when
            // the guest heals / recovery lands.
            Unreachable => matches!(
                target,
                Active | Idle | HostLost | Evicting | Failed | Completed | Dead
            ),
            // ADR 0077 phase 4 (Revive): Idle -> Active directly — a
            // successful resume binds the sandbox and reattaches the
            // harness, then flips straight to Active in ONE step, with
            // no `Created` limbo (Created stays the start_agent-FAILED
            // resting state, /exec 409). Created remains legal for the
            // create path and for the harness-failed resume arm.
            Idle => matches!(target, Active | Created | Dead | Completed | Queued),
            HostLost => matches!(target, Created | Idle | Dead | Completed),
            Evacuating => matches!(target, Created | Idle | Dead | Completed),
            // ADR 0074 rung 1: `Active` is the cancel edge — a returning
            // user's prompt un-nominates an eviction whose capture has
            // not begun (lease-guarded CAS; see
            // api::snapshot::try_cancel_nominated_eviction). The ONLY
            // new FSM edge in the 2026-07 overhaul.
            Evicting => matches!(target, Active | Idle | HostLost | Dead | Completed),
            Failed | Completed | Dead => false,
        }
    }

    /// The FSM-legal terminal a forced termination (operator delete, drain,
    /// give-up) should drive this state to: `Completed` for states that
    /// actually ran, `Failed` for the early states that never became
    /// usable, `None` if already terminal (nothing to do). Single source of
    /// truth for "what terminal does terminating this session mean" — kept
    /// beside [`Self::can_transition_to`] so it can't drift from the table
    /// above (and debug-asserted to be a legal edge).
    pub fn terminal_target(&self) -> Option<Self> {
        use SessionState::*;
        let target = match self {
            Active | Unreachable | Idle | HostLost | Evacuating | Evicting => Completed,
            // Queued never ran → Failed, alongside the other never-usable
            // early states.
            Pending | Queued | Created => Failed,
            Failed | Completed | Dead => return None,
        };
        debug_assert!(
            self.can_transition_to(target),
            "terminal_target({self:?}) = {target:?} must be a legal transition",
        );
        Some(target)
    }

    /// Consume `self` and produce the next state if the transition is
    /// legal. Used by [`MetadataStore::transition_session`] (engram-
    /// postgres) to gate every `UPDATE sessions SET status = ...` —
    /// every persistence-layer write to `sessions.status` flows
    /// through this check.
    pub fn try_transition_to(self, target: Self) -> Result<Self, IllegalTransition> {
        if self.can_transition_to(target) {
            Ok(target)
        } else {
            Err(IllegalTransition {
                from: self,
                to: target,
            })
        }
    }
}

/// Error returned by [`SessionState::try_transition_to`] when the
/// requested move is not in the legality table. Carries both sides so
/// the error surfaces in logs and HTTP bodies without the caller
/// having to reconstruct context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IllegalTransition {
    pub from: SessionState,
    pub to: SessionState,
}

impl fmt::Display for IllegalTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "illegal session state transition: {} -> {}",
            self.from.as_str(),
            self.to.as_str()
        )
    }
}

impl std::error::Error for IllegalTransition {}

/// Full OCI reference (registry host + repo path + tag) for the
/// image this session boots from. Examples:
///   - `ghcr.io/cortex/api:warm-2026-01-01T00:00:00Z`
///   - `localhost:5001/cortex/api:warm-X` (dev registry)
///   - `us-east1-docker.pkg.dev/proj/repo/img:v1` (GAR)
///
/// Engram doesn't maintain a curated catalog at the `(repo, tag)`
/// granularity — operators explicitly enable URIs via
/// `enabled_images`, sessions reference them by the full URI, and
/// the host-agent pulls them via the auth resolver.
pub type ImageRef = String;

/// Split a `host[:port]/repo[/path]:tag` reference into
/// `(host_and_path, tag)`. Used by `SecretContext` namespacing and
/// `SnapshotRecord` labelling — places that want either the path
/// portion (without the tag) or just the tag.
///
/// Returns `(uri, "")` if there's no `:tag` suffix (a digest-only
/// reference uses `@sha256:...` syntax which we don't decompose
/// here).
/// ADR 0048: whether a queued session is waiting to boot a fresh create
/// or to resume an idle session — they have different dequeue + timeout
/// targets (`Queued → Pending` vs `Queued → Idle`; timeout → `Failed`
/// vs back to `Idle`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueOrigin {
    Create,
    Resume,
}

impl QueueOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Resume => "resume",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "create" => Some(Self::Create),
            "resume" => Some(Self::Resume),
            _ => None,
        }
    }
}

/// ADR 0048: a row the queue scanner sees — the session plus its queue
/// metadata (origin, the budgets to reserve with, and when it was queued
/// for FIFO + timeout). The create-time prompt is NOT here: it was enqueued
/// to the durable outbox at create (ADR 0073), so the scanner only needs to
/// boot the session for the delivery driver to forward it.
#[derive(Clone, Debug)]
pub struct QueuedSession {
    pub session: Session,
    pub origin: QueueOrigin,
    pub mem_budget_mib: i64,
    pub cpu_budget_vcpus: i32,
    pub queued_at: DateTime<Utc>,
}

/// ADR 0048 C8: an Active session bound to a host, with its reservation
/// budgets — the drain don't-strand guard needs the budgets to ask
/// "does some survivor fit this session?".
#[derive(Clone, Copy, Debug)]
pub struct SandboxAssignment {
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    pub mem_budget_mib: i64,
    pub cpu_budget_vcpus: i32,
}

/// ADR 0048: outcome of [`crate::traits::MetadataStore::delete_host`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeleteHostOutcome {
    /// The host row was deleted (or was already gone — idempotent).
    Deleted,
    /// Refused: `n` sessions are still bound to the host.
    SessionsBound(u64),
}

/// ADR 0048: the queue's aggregate demand — the autoscaler's scale-up
/// signal (`/admin/fleet/demand`).
#[derive(Clone, Copy, Debug, Default)]
pub struct QueuedDemand {
    pub sessions: u64,
    pub mem_mib: u64,
    pub vcpus: u64,
}

pub fn split_image_ref(uri: &str) -> (&str, &str) {
    // Find the LAST ':' AFTER the last '/' — protects against
    // splitting on the registry's port (e.g. `localhost:5001/...`).
    if let Some(slash) = uri.rfind('/') {
        if let Some(colon_in_tail) = uri[slash..].rfind(':') {
            let cut = slash + colon_in_tail;
            return (&uri[..cut], &uri[cut + 1..]);
        }
    } else if let Some(colon) = uri.rfind(':') {
        return (&uri[..colon], &uri[colon + 1..]);
    }
    (uri, "")
}

/// How a session uses its image (ADR 0021 P1.3). Whether a harness
/// runs at all is now an *image* property (the `[harness]` block in
/// `engram.toml`); the session only chooses whether the host actually
/// drives the resident harness or treats the VM as a shell-only dev
/// machine.
///
/// For a harness-less image (no `[harness]` block) `Agent` is
/// effectively the same as `DevVm` — there's nothing for the host to
/// drive — but the surface stays uniform: the session-create API
/// always carries a `mode`, and the legacy `HarnessSpec` selection
/// per session is gone (the harness is baked at image-bake time, not
/// session-create time).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// Run the image's baked harness (if any). The host calls
    /// `SpawnHarness` with the manifest-declared argv; the resident
    /// agent serves the session.
    #[default]
    Agent,
    /// Boot the image as a pure dev VM. If the image has a baked
    /// harness, it's left resident in the warm snapshot but never
    /// driven — `SpawnHarness` arrives with empty argv as a readiness
    /// probe, just like the old `HarnessSpec::None` path. Sessions
    /// interact via shell / `engram exec`.
    DevVm,
}

impl SessionMode {
    /// True for the dev-VM case — no harness child spawn, regardless
    /// of whether the image has one baked in.
    pub fn is_dev_vm(self) -> bool {
        matches!(self, Self::DevVm)
    }

    /// Wire-string used in the DB (`sessions.mode` TEXT column,
    /// CHECK-constrained to this exact set in migration 0039) and in
    /// the HTTP API. Matches the serde `rename_all = "snake_case"`
    /// output for the same variants.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::DevVm => "dev_vm",
        }
    }
}

/// User-facing request to create a session. Two axes:
/// image (immutable rootfs that supplies the workspace + the optional
/// baked harness) + mode (drive the harness or treat the VM as a dev
/// machine). ADR 0005 retired the `WorkspaceSpec` axis; ADR 0021 P1.3
/// retired the per-session `HarnessSpec` axis — the harness is an
/// image property now.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSpec {
    pub image: ImageRef,
    #[serde(default)]
    pub mode: SessionMode,
}

/// A persisted session row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub status: SessionState,
    pub host_id: Option<HostId>,
    /// In-memory `SandboxId` of the live sandbox serving this
    /// session. Populated from `Created` onward; cleared back to
    /// `None` on `Idle` (sandbox evicted), `HostLost` (host gone),
    /// terminal states.
    #[serde(default)]
    pub sandbox_id: Option<SandboxId>,
    pub image: ImageRef,
    #[serde(default)]
    pub mode: SessionMode,
    pub created_at: DateTime<Utc>,
    pub last_active_at: DateTime<Utc>,
    /// ADR 0016 Phase B: the host's last-published live disk
    /// manifest from the FlushScheduler. Updated by
    /// `MetadataStore::update_live_disk_manifest`; cleared by
    /// `assign_session_sandbox(None)`. Coord's
    /// `effective_resume_disk_manifest` picks the newer of this
    /// and `snapshots.disk_manifest` so the first resume after
    /// continuous flush is enabled doesn't silently roll back to
    /// the snapshot's stale disk lineage. `None` for sessions
    /// that haven't gotten a publish (non-NBD hosts /
    /// pre-Phase-B sessions).
    #[serde(default)]
    pub live_disk_manifest: Option<crate::types::manifest::ManifestRef>,
    /// ADR 0074 parking ladder: which rung this session's sandbox is
    /// currently parked at. `0` = not parked (normal). `1` = nominated
    /// for eviction (still Active, cancellable). `2` = parked-paused
    /// (VM paused in place, sandbox bound, un-pause to ascend). `3` =
    /// parked-local (data plane retained on-host after destroy). The
    /// coordinator reads this on the cancel/ascent path to choose how
    /// to bring the session back (un-pause vs. resume vs. rebuild).
    #[serde(default)]
    pub park_rung: i16,
    /// ADR 0074: when this session entered its current parking rung.
    /// Drives the rung-2 dwell cap (reclaim a paused VM's RAM if the
    /// user hasn't returned within the cap) and rung-3 retention
    /// accounting. `None` when `park_rung == 0`.
    #[serde(default)]
    pub parked_at: Option<DateTime<Utc>>,
    /// Session titles: the latest harness-suggested (LLM-generated) title for
    /// this session, or `None` until the harness emits one. Written in real
    /// time by the coordinator's harness event sink; the orchestrator reads it
    /// (via the Session proto) as a task's display title. A self-descriptive
    /// operational fact, never user attribution.
    #[serde(default)]
    pub suggested_title: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_state_serializes_lowercase() {
        let payload = serde_json::to_value(SessionState::Active).unwrap();
        assert_eq!(payload, serde_json::json!("active"));
        let parsed: SessionState = serde_json::from_str(r#""idle""#).unwrap();
        assert_eq!(parsed, SessionState::Idle);
    }

    #[test]
    fn session_state_new_variants_round_trip() {
        // Wire shape for the three M2 additions. Anything that
        // hardcodes the variant order or spelling on the client side
        // (UI, integration scripts) must match these strings.
        for (variant, wire) in [
            (SessionState::Created, "created"),
            (SessionState::HostLost, "host_lost"),
        ] {
            assert_eq!(
                serde_json::to_value(variant).unwrap(),
                serde_json::json!(wire)
            );
            let back: SessionState = serde_json::from_str(&format!("\"{wire}\"")).unwrap();
            assert_eq!(back, variant);
            assert_eq!(variant.as_str(), wire);
        }
    }

    #[test]
    fn session_state_unknown_string_rejected() {
        let res: Result<SessionState, _> = serde_json::from_str(r#""running""#);
        assert!(res.is_err(), "unknown variants must fail to deserialize");
    }

    #[test]
    fn session_state_as_str_matches_serde_form() {
        for s in [
            SessionState::Pending,
            SessionState::Queued,
            SessionState::Created,
            SessionState::Active,
            SessionState::Idle,
            SessionState::HostLost,
            SessionState::Evacuating,
            SessionState::Evicting,
            SessionState::Completed,
            SessionState::Failed,
            SessionState::Dead,
        ] {
            let via_serde = serde_json::to_string(&s).unwrap();
            let trimmed = via_serde.trim_matches('"');
            assert_eq!(s.as_str(), trimmed, "as_str must match wire format");
        }
    }

    /// The full ADR 0015 M2 legality table. Every (from, to) pair we
    /// allow must round-trip through `try_transition_to`; every pair
    /// we don't allow must fail. The point is to make the table
    /// itself the test contract — if you add an edge, you update this
    /// test and `can_transition_to` together.
    #[test]
    fn legality_table_matches_adr() {
        use SessionState::*;
        let allowed: &[(SessionState, SessionState)] = &[
            (Pending, Created),
            (Pending, Failed),
            (Pending, Queued),
            (Queued, Pending),
            (Queued, Idle),
            (Queued, Failed),
            (Idle, Queued),
            (Created, Active),
            (Created, Failed),
            (Created, HostLost),
            (Active, Idle),
            (Active, HostLost),
            (Active, Evacuating),
            (Active, Evicting),
            (Active, Failed),
            (Active, Completed),
            (Active, Dead),
            (Idle, Created),
            (Idle, Dead),
            (Idle, Completed),
            (HostLost, Created),
            (HostLost, Idle),
            (HostLost, Dead),
            (HostLost, Completed),
            (Evacuating, Created),
            (Evacuating, Idle),
            (Evacuating, Dead),
            (Evacuating, Completed),
            // ADR 0074 rung 1: the cancel edge — the ONLY new FSM edge
            // in the 2026-07 overhaul (lease-guarded; see
            // try_cancel_nominated_eviction).
            (Evicting, Active),
            // ADR 0077 phase 4: the Revive edge — a resumed session
            // reattaches its harness and flips Idle -> Active directly.
            (Idle, Active),
            (Evicting, Idle),
            (Evicting, HostLost),
            (Evicting, Dead),
            (Evicting, Completed),
        ];
        let all_states = [
            Pending, Queued, Created, Active, Idle, HostLost, Evacuating, Evicting, Failed,
            Completed, Dead,
        ];
        for &from in &all_states {
            for &to in &all_states {
                let want = allowed.contains(&(from, to));
                assert_eq!(
                    from.can_transition_to(to),
                    want,
                    "({from:?} -> {to:?}) expected {want}"
                );
                let got = from.try_transition_to(to);
                if want {
                    assert_eq!(got, Ok(to));
                } else {
                    assert_eq!(got, Err(IllegalTransition { from, to }));
                }
            }
        }
    }

    #[test]
    fn terminal_states_reject_all_outgoing() {
        use SessionState::*;
        for terminal in [Failed, Completed, Dead] {
            assert!(terminal.is_terminal());
            for target in [
                Pending, Queued, Created, Active, Idle, HostLost, Evacuating, Evicting,
            ] {
                assert_eq!(
                    terminal.try_transition_to(target),
                    Err(IllegalTransition {
                        from: terminal,
                        to: target,
                    })
                );
            }
        }
    }

    #[test]
    fn terminal_target_is_legal_and_total() {
        use SessionState::*;
        // Every non-terminal state maps to a terminal via a LEGAL edge.
        for &s in &[
            Pending, Created, Active, Idle, HostLost, Evacuating, Evicting,
        ] {
            let target = s
                .terminal_target()
                .expect("a non-terminal state must have a terminal_target");
            assert!(target.is_terminal(), "{s:?} -> {target:?} must be terminal");
            assert!(
                s.can_transition_to(target),
                "{s:?} -> {target:?} must be a legal FSM edge",
            );
        }
        // Terminal states have nothing to terminate.
        for &t in &[Failed, Completed, Dead] {
            assert_eq!(t.terminal_target(), None, "{t:?} is already terminal");
        }
        // The semantic split: states that ran complete; never-ran ones fail.
        assert_eq!(Active.terminal_target(), Some(Completed));
        assert_eq!(Idle.terminal_target(), Some(Completed));
        assert_eq!(Created.terminal_target(), Some(Failed));
        assert_eq!(Pending.terminal_target(), Some(Failed));
    }

    #[test]
    fn illegal_transition_displays_both_sides() {
        let err = IllegalTransition {
            from: SessionState::Active,
            to: SessionState::Pending,
        };
        let rendered = format!("{err}");
        assert!(rendered.contains("active"), "rendered: {rendered}");
        assert!(rendered.contains("pending"), "rendered: {rendered}");
    }

    #[test]
    fn split_image_ref_handles_typical_shapes() {
        // GHCR-style: host + multi-segment repo + tag.
        let (rest, tag) = split_image_ref("ghcr.io/cortex/api:warm-2026");
        assert_eq!(rest, "ghcr.io/cortex/api");
        assert_eq!(tag, "warm-2026");

        // Local registry with port — port colon must NOT be treated
        // as the tag separator.
        let (rest, tag) = split_image_ref("localhost:5001/cortex/api:warm-X");
        assert_eq!(rest, "localhost:5001/cortex/api");
        assert_eq!(tag, "warm-X");

        // No tag → returns whole input as `rest`, empty `tag`.
        let (rest, tag) = split_image_ref("ghcr.io/cortex/api");
        assert_eq!(rest, "ghcr.io/cortex/api");
        assert_eq!(tag, "");
    }

    #[test]
    fn session_mode_default_is_agent() {
        // Default is the "drive the baked harness" case so a
        // SessionSpec deserialized from a payload that omits `mode`
        // matches today's expectation: hosts drive the harness when
        // there's one to drive.
        assert_eq!(SessionMode::default(), SessionMode::Agent);
        assert!(!SessionMode::Agent.is_dev_vm());
        assert!(SessionMode::DevVm.is_dev_vm());
    }

    #[test]
    fn session_mode_round_trips_through_json() {
        // Wire shape is the flat snake_case string — `"agent"` /
        // `"dev_vm"` — chosen so the API surface stays human-readable
        // and the dashboard form can use it directly.
        for (mode, expected) in [
            (SessionMode::Agent, "\"agent\""),
            (SessionMode::DevVm, "\"dev_vm\""),
        ] {
            let blob = serde_json::to_string(&mode).unwrap();
            assert_eq!(blob, expected);
            let back: SessionMode = serde_json::from_str(&blob).unwrap();
            assert_eq!(back, mode);
        }
    }

    #[test]
    fn image_ref_round_trips_through_json() {
        // ImageRef is now a flat String URI; serialization is just
        // the string. Pin the wire shape so changes that move it
        // back to a tagged enum fail loudly.
        let img: ImageRef = "ghcr.io/cortex/api:warm-2026".to_string();
        let blob = serde_json::to_string(&img).unwrap();
        assert_eq!(blob, "\"ghcr.io/cortex/api:warm-2026\"");
        let back: ImageRef = serde_json::from_str(&blob).unwrap();
        assert_eq!(back, img);
    }

    #[test]
    fn session_round_trips_through_json() {
        let original = Session {
            id: SessionId::new(),
            status: SessionState::Active,
            host_id: Some(HostId::new()),
            sandbox_id: Some(SandboxId::new()),
            image: "ghcr.io/cortex/api:warm-20260101T000000Z".into(),
            mode: SessionMode::DevVm,
            created_at: Utc::now(),
            last_active_at: Utc::now(),
            live_disk_manifest: None,
            park_rung: 0,
            parked_at: None,
            suggested_title: None,
        };
        let blob = serde_json::to_string(&original).unwrap();
        let back: Session = serde_json::from_str(&blob).unwrap();
        assert_eq!(back.id, original.id);
        assert_eq!(back.image, original.image);
        assert_eq!(back.mode, original.mode);
    }
}
