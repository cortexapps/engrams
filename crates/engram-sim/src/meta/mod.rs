//! `SimMetadataStore` — an in-memory `MetadataStore` faithful to
//! `PostgresStore`'s observable semantics (ADR 0098 D4).
//!
//! # The fidelity contract
//!
//! Each `MetadataStore` method corresponds to one SQL statement or one
//! transaction in `PostgresStore` — the trait was designed so no app
//! logic sits between round trips of a single method. Every method here
//! therefore takes ONE `parking_lot::Mutex<SimDb>` lock, mutates
//! synchronously, and unlocks — never holding the lock across an await
//! (there are none). That reproduces PG's per-statement/per-transaction
//! atomicity exactly; the nondeterminism PG exhibits (which replica's
//! `FOR UPDATE SKIP LOCKED` claim wins, `ON CONFLICT` race winners) is
//! reproduced by the *scheduler's interleaving of method calls*, not
//! modeled inside the store.
//!
//! All collections are `BTreeMap`/`BTreeSet` — deterministic iteration
//! is load-bearing; never add a HashMap. `serial` mimics BIGSERIAL for
//! ordering tie-breaks.
//!
//! Methods NOT implemented panic loudly (see `sim_unimplemented!`)
//! instead of inheriting the trait's no-op defaults, so a newly-driven
//! code path fails at the call site rather than silently observing an
//! empty default.
//!
//! **Process rule:** any PR adding a `MetadataStore` method or changing
//! `PostgresStore` SQL semantics must extend `tests/` (the conformance
//! suite, which runs each scenario against BOTH stores) in the same PR.
//!
//! Note on `InMemoryOpLog` (engram-core): deliberately NOT reused here.
//! It carries its own mutex; `op_enqueue_and_claim` must bump
//! `sessions.current_epoch` and mutate the op row in ONE atomic unit
//! (one PG transaction), and composing two locks would open an
//! interleaving window between them that PG does not have.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use engram_core::traits::{Clock, Entropy};
use engram_core::MetaError;
use parking_lot::Mutex;

mod store_impl;

/// A would-be `pg_notify` recorded instead of fired. The D5 scheduler
/// drains these and decides delivery (or dropping) explicitly; the
/// polling drivers must converge without them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimNotify {
    pub channel: &'static str,
    pub payload: String,
}

pub struct SimMetadataStore {
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) entropy: Arc<dyn Entropy>,
    pub(crate) db: Mutex<SimDb>,
    /// Fault hook: while set, every method returns the retryable
    /// `MetaError::Db` shape the drivers already classify (a simulated
    /// PG outage window).
    outage: AtomicBool,
    notifications: Mutex<VecDeque<SimNotify>>,
}

impl std::fmt::Debug for SimMetadataStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimMetadataStore").finish_non_exhaustive()
    }
}

impl SimMetadataStore {
    pub fn new(clock: Arc<dyn Clock>, entropy: Arc<dyn Entropy>) -> Arc<Self> {
        Arc::new(Self {
            clock,
            entropy,
            db: Mutex::new(SimDb::default()),
            outage: AtomicBool::new(false),
            notifications: Mutex::new(VecDeque::new()),
        })
    }

    /// Simulated PG outage window: while set, every trait method fails
    /// with a retryable `MetaError::Db`.
    pub fn set_outage(&self, on: bool) {
        self.outage.store(on, Ordering::SeqCst);
    }

    /// Drain the recorded would-be `pg_notify` events.
    pub fn drain_notifications(&self) -> Vec<SimNotify> {
        self.notifications.lock().drain(..).collect()
    }

    /// Direct read access for invariant checkers (D5+). Callers must be
    /// synchronous and must not re-enter the store while holding this.
    pub fn with_db<R>(&self, f: impl FnOnce(&SimDb) -> R) -> R {
        f(&self.db.lock())
    }

    /// Direct MUTABLE access — for tests that inject corruption to prove
    /// an oracle is non-vacuous (e.g. drop a live session row and assert
    /// the model auditor fires). Not used by production drivers.
    pub fn with_db_mut<R>(&self, f: impl FnOnce(&mut SimDb) -> R) -> R {
        f(&mut self.db.lock())
    }

    pub(crate) fn gate(&self) -> Result<(), MetaError> {
        if self.outage.load(Ordering::SeqCst) {
            return Err(MetaError::Db("sim: pg outage window".into()));
        }
        Ok(())
    }

    pub(crate) fn notify(&self, channel: &'static str, payload: impl Into<String>) {
        self.notifications.lock().push_back(SimNotify {
            channel,
            payload: payload.into(),
        });
    }

    pub(crate) fn now(&self) -> DateTime<Utc> {
        self.clock.now_utc()
    }
}

// ---------------------------------------------------------------------------
// The database.
// ---------------------------------------------------------------------------

use engram_core::types::capture_job::CaptureJobRow;
use engram_core::types::event::{ArtifactRow, PersistedEvent};
use engram_core::types::host::HostRecord;
use engram_core::types::outbox::OutboxRow;
use engram_core::types::registry::EnableJob;
use engram_core::types::registry::{EnabledImage, RegistryCredential, SessionSecrets};
use engram_core::types::session::{QueueOrigin, Session};
use engram_core::types::session_op::{OpKind, OpState};
use engram_core::types::snapshot::SnapshotRecord;
use engram_core::{HostId, SessionId, SnapshotId};

/// `sessions` row: the domain `Session` plus the columns PostgresStore
/// keeps that the domain type doesn't carry.
#[derive(Clone, Debug)]
pub struct SessRow {
    pub session: Session,
    pub mem_budget_mib: i64,
    pub cpu_budget_vcpus: i32,
    pub queue_origin: Option<QueueOrigin>,
    pub queued_at: Option<DateTime<Utc>>,
    pub missing_strikes: i32,
    pub current_epoch: i64,
    pub binding_epoch: i64,
    pub next_event_idx: i64,
    pub recovery_epoch: i64,
    pub shell_pinned_until: Option<DateTime<Utc>>,
    pub durable_head: Option<SnapshotId>,
    pub evac_attempts: i32,
    pub evict_attempts: i32,
    pub updated_at: DateTime<Utc>,
}

/// `session_ops` row: the domain `SessionOp` fields plus the DB-only
/// lease bookkeeping (`claimed_at`, `heartbeat_at`).
#[derive(Clone, Debug)]
pub struct OpRow {
    pub id: i64,
    pub session_id: SessionId,
    pub kind: OpKind,
    pub payload: serde_json::Value,
    pub state: OpState,
    pub step: Option<String>,
    pub epoch: Option<i64>,
    pub attempts: i32,
    pub not_before: Option<DateTime<Utc>>,
    pub idempotency_key: Option<String>,
    pub claimed_by: Option<String>,
    pub claimed_at: Option<DateTime<Utc>>,
    pub heartbeat_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub struct GcCandidate {
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct EnableJobRow {
    pub job: EnableJob,
    pub claimed_by: Option<String>,
    pub claimed_at: Option<DateTime<Utc>>,
}

#[derive(Default)]
pub struct SimDb {
    pub serial: u64,
    pub sessions: std::collections::BTreeMap<SessionId, SessRow>,
    pub hosts: std::collections::BTreeMap<HostId, HostRecord>,
    pub snapshots: std::collections::BTreeMap<SnapshotId, SnapshotRecord>,
    pub session_events: std::collections::BTreeMap<SessionId, Vec<PersistedEvent>>,
    pub artifacts: std::collections::BTreeMap<(SessionId, uuid::Uuid), ArtifactRow>,
    pub registry_credentials: std::collections::BTreeMap<String, RegistryCredential>,
    /// `image_uri -> (image, soft_deleted_at)`.
    pub enabled_images: std::collections::BTreeMap<String, (EnabledImage, Option<DateTime<Utc>>)>,
    pub enable_jobs: std::collections::BTreeMap<uuid::Uuid, EnableJobRow>,
    pub capture_jobs: std::collections::BTreeMap<engram_core::CaptureJobId, CaptureJobRow>,
    pub session_secrets: std::collections::BTreeMap<SessionId, SessionSecrets>,
    /// ADR 0023 in-guest forge broker tokens, keyed by session (PG:
    /// `session_broker_tokens`, PK `session_id`).
    pub broker_tokens:
        std::collections::BTreeMap<SessionId, engram_core::types::registry::SessionBrokerToken>,
    /// ADR 0106: sealed payloads keyed by opaque subject + provider.
    pub oauth_credentials: std::collections::BTreeMap<
        engram_core::types::oauth::OAuthCredentialKey,
        engram_core::types::oauth::SealedOAuthCredential,
    >,
    /// Advisory refresh claims (PG: `oauth_credentials.refresh_claim_until`).
    /// Kept beside the rows because the claim is store-internal scheduling
    /// state, not part of the domain credential type.
    pub oauth_refresh_claims:
        std::collections::BTreeMap<engram_core::types::oauth::OAuthCredentialKey, DateTime<Utc>>,
    pub oauth_flows: std::collections::BTreeMap<uuid::Uuid, engram_core::types::oauth::OAuthFlow>,
    pub session_oauth_bindings:
        std::collections::BTreeMap<SessionId, engram_core::types::oauth::SessionOAuthBinding>,
    /// ADR 0045 live-migration teleport pin (PG: `sessions.
    /// teleport_target_host_id` + `_set_at`). Present only while a pin is
    /// set. The sim runs no teleport workload today, but the boot/rebind
    /// path clears the pin, so get/set must round-trip.
    pub teleport_targets: std::collections::BTreeMap<SessionId, (HostId, Option<DateTime<Utc>>)>,
    pub session_ops: std::collections::BTreeMap<i64, OpRow>,
    pub outbox: std::collections::BTreeMap<String, OutboxRow>,
    pub bundle_gc: std::collections::BTreeMap<String, GcCandidate>,
    pub snapshot_blob_gc: std::collections::BTreeMap<SnapshotId, GcCandidate>,
    pub chunk_gc: std::collections::BTreeMap<Vec<u8>, GcCandidate>,
    pub chunk_generation: u64,
    /// `dead_host_inflight`: host -> (claimed_by, claimed_at).
    pub dead_host_inflight: std::collections::BTreeMap<HostId, (String, DateTime<Utc>)>,
    pub runtime_specs:
        std::collections::BTreeMap<SessionId, engram_core::types::runtime_spec::RuntimeSpec>,
    pub session_capabilities:
        std::collections::BTreeMap<SessionId, Vec<engram_core::types::capability::Capability>>,
    pub session_integration_policy: std::collections::BTreeMap<SessionId, String>,
    /// `cold_bases`: snapshot ids pinned as cold base images.
    pub cold_bases: std::collections::BTreeSet<SnapshotId>,
    /// Every session-status flip this store performed, in order — the
    /// D6 transition-legality oracle's input (defense-in-depth over the
    /// FSM checks in the write paths, and it catches direct-write bugs
    /// in SimMeta itself). `exempt` marks the documented
    /// mark_host_dead_and_orphan_sessions bulk flip, which is broader
    /// than the FSM table (ADR 0099 H6 finding, design call pending).
    pub transition_log: Vec<TransitionLogEntry>,
}

#[derive(Clone, Debug)]
pub struct TransitionLogEntry {
    pub session: SessionId,
    pub from: engram_core::types::session::SessionState,
    pub to: engram_core::types::session::SessionState,
    pub exempt: bool,
}

impl SimDb {
    pub(crate) fn next_serial(&mut self) -> u64 {
        self.serial += 1;
        self.serial
    }
}
