//! ADR 0079 (issue #543): the durable per-session op log — the lifecycle
//! kernel's row types.
//!
//! Every session lifecycle verb is a `session_ops` row driven by a
//! single-writer-per-session executor. `sessions.current_epoch` is the
//! fencing epoch: CAS-bumped in the same transaction that claims an op,
//! stamped into every PG session-write and every session-scoped host RPC.
//! At most one `running` op per session (a partial unique index enforces
//! it); queued ops execute strictly in `id` order.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ids::SessionId;

/// The lifecycle verb a row represents. String-stable (PG `kind` column);
/// parse/format round-trips exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    /// Boot a placed/queued session (the queue scanner's drive verb).
    CreateBoot,
    /// Bring an Idle/parked session back to Active.
    Resume,
    /// The idle-eviction pipeline (park or capture+destroy+mark-idle).
    Evict,
    /// Forward the head outbox row (ordering only — the outbox itself
    /// stays ADR 0073's).
    Deliver,
    /// Tear the session down (DELETE /session).
    Destroy,
    /// A manual snapshot / checkpoint-finalize claim. Currently only
    /// inline-driven (the manual `/snapshot` endpoint claims it for
    /// exclusion); becomes a real verb when the ADR 0069/0077 finalize
    /// machinery migrates.
    CheckpointFinalize,
    /// A live post-copy migration (ADR 0045 C2). Currently only
    /// inline-driven (`live_migration.rs` claims it for exclusion);
    /// becomes a real verb in a follow-up phase.
    Teleport,
}

impl OpKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CreateBoot => "create_boot",
            Self::Resume => "resume",
            Self::Evict => "evict",
            Self::Deliver => "deliver",
            Self::Destroy => "destroy",
            Self::CheckpointFinalize => "checkpoint_finalize",
            Self::Teleport => "teleport",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "create_boot" => Self::CreateBoot,
            "resume" => Self::Resume,
            "evict" => Self::Evict,
            "deliver" => Self::Deliver,
            "destroy" => Self::Destroy,
            "checkpoint_finalize" => Self::CheckpointFinalize,
            "teleport" => Self::Teleport,
            _ => return None,
        })
    }
}

/// Row lifecycle. `queued → running → done|failed`, `queued → cancelled`,
/// `running → queued` (requeue-with-backoff on retryable failure).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl OpState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }
}

/// A `session_ops` row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionOp {
    pub id: i64,
    pub session_id: SessionId,
    pub kind: OpKind,
    pub payload: serde_json::Value,
    pub state: OpState,
    /// Verb-specific durable step marker — the resume point after a
    /// reclaim. `None` until the first `record_op_step`.
    pub step: Option<String>,
    /// The fencing epoch assigned at claim (`sessions.current_epoch`'s
    /// post-bump value). `None` while queued.
    pub epoch: Option<i64>,
    pub attempts: i32,
    pub not_before: Option<DateTime<Utc>>,
    pub idempotency_key: Option<String>,
    pub claimed_by: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// Outcome of an enqueue: `Claimed` means the same transaction also
/// claimed the op (nothing was running for the session — the happy path
/// is one PG round trip) and the caller-executor should drive it now.
#[derive(Clone, Debug)]
pub enum EnqueueOutcome {
    /// Inserted and immediately claimed; carries the claimed row (epoch
    /// stamped).
    Claimed(SessionOp),
    /// Inserted behind an in-flight or queued op; the executor picks it
    /// up in order.
    Queued(SessionOp),
    /// An identical (session, kind, idempotency_key) row already exists
    /// and is not finished — the enqueue is a no-op.
    Duplicate,
}

/// ADR 0079: reference in-memory implementation of the op-log store
/// surface, for MOCK `MetadataStore`s (the coordinator's `MiniMeta` and
/// the integration-test mocks embed one and delegate). Minimal but
/// honest: one running op per session, epoch CAS-bump on claim,
/// idempotency-key dedup, and fenced (`epoch` + `state = running`)
/// step/finish/requeue writes — the same semantics `PostgresStore`
/// implements in SQL (pinned by `session_ops_live_pg.rs`). No reclaim
/// sweep: mocks never simulate a dead executor's stale heartbeat.
#[derive(Default)]
pub struct InMemoryOpLog {
    inner: std::sync::Mutex<OpLogInner>,
}

#[derive(Default)]
struct OpLogInner {
    next_id: i64,
    ops: Vec<SessionOp>,
    epochs: std::collections::HashMap<SessionId, i64>,
}

impl InMemoryOpLog {
    /// The session's current fencing epoch (0 = never claimed) — the
    /// value mock `fenced_*` writes compare against.
    pub fn current_epoch(&self, session_id: SessionId) -> i64 {
        *self
            .inner
            .lock()
            .unwrap()
            .epochs
            .get(&session_id)
            .unwrap_or(&0)
    }

    pub fn enqueue_and_claim(
        &self,
        session_id: SessionId,
        kind: OpKind,
        payload: serde_json::Value,
        idempotency_key: Option<&str>,
        claimed_by: &str,
    ) -> EnqueueOutcome {
        let mut inner = self.inner.lock().unwrap();
        // Idempotency: mirror the PG partial unique (session, kind, key)
        // scoped to the ACTIVE states (ADR 0079 review finding #4) — a
        // TERMINAL keyed op's key is NOT burned, so a fresh enqueue after
        // a terminal failure lands a new row (the scanner/reaper can
        // re-drive; a terminally-failed keyed evict no longer wedges the
        // session forever).
        if let Some(key) = idempotency_key {
            if inner.ops.iter().any(|o| {
                o.session_id == session_id
                    && o.kind == kind
                    && o.idempotency_key.as_deref() == Some(key)
                    && matches!(o.state, OpState::Queued | OpState::Running)
            }) {
                return EnqueueOutcome::Duplicate;
            }
        }
        inner.next_id += 1;
        let id = inner.next_id;
        let mut op = SessionOp {
            id,
            session_id,
            kind,
            payload,
            state: OpState::Queued,
            step: None,
            epoch: None,
            attempts: 0,
            not_before: None,
            idempotency_key: idempotency_key.map(String::from),
            claimed_by: None,
            error: None,
            created_at: Utc::now(),
            finished_at: None,
        };
        let claimable = !inner.ops.iter().any(|o| {
            o.session_id == session_id
                && (o.state == OpState::Running || (o.state == OpState::Queued && o.id < id))
        });
        if claimable {
            let epoch = inner.epochs.entry(session_id).or_insert(0);
            *epoch += 1;
            op.state = OpState::Running;
            op.epoch = Some(*epoch);
            op.attempts = 1;
            op.claimed_by = Some(claimed_by.to_string());
            let claimed = op.clone();
            inner.ops.push(op);
            EnqueueOutcome::Claimed(claimed)
        } else {
            let queued = op.clone();
            inner.ops.push(op);
            EnqueueOutcome::Queued(queued)
        }
    }

    /// ADR 0079 (review finding #8): atomic claim-or-fail for inline
    /// claims — claim IFF the lane is free (nothing running, nothing else
    /// queued), else leave NO row (mirrors the PG rollback). `None` =
    /// busy.
    pub fn enqueue_and_claim_exclusive(
        &self,
        session_id: SessionId,
        kind: OpKind,
        payload: serde_json::Value,
        claimed_by: &str,
    ) -> Option<SessionOp> {
        let mut inner = self.inner.lock().unwrap();
        let free = !inner.ops.iter().any(|o| {
            o.session_id == session_id && matches!(o.state, OpState::Running | OpState::Queued)
        });
        if !free {
            return None;
        }
        inner.next_id += 1;
        let id = inner.next_id;
        let epoch = {
            let e = inner.epochs.entry(session_id).or_insert(0);
            *e += 1;
            *e
        };
        let op = SessionOp {
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
            error: None,
            created_at: Utc::now(),
            finished_at: None,
        };
        let claimed = op.clone();
        inner.ops.push(op);
        Some(claimed)
    }

    pub fn claim_head(&self, session_id: SessionId, claimed_by: &str) -> Option<SessionOp> {
        let mut inner = self.inner.lock().unwrap();
        if inner
            .ops
            .iter()
            .any(|o| o.session_id == session_id && o.state == OpState::Running)
        {
            return None;
        }
        let now = Utc::now();
        let head_id = inner
            .ops
            .iter()
            .filter(|o| {
                o.session_id == session_id
                    && o.state == OpState::Queued
                    && o.not_before.is_none_or(|nb| nb <= now)
            })
            .map(|o| o.id)
            .min()?;
        let epoch = {
            let e = inner.epochs.entry(session_id).or_insert(0);
            *e += 1;
            *e
        };
        let op = inner.ops.iter_mut().find(|o| o.id == head_id).unwrap();
        op.state = OpState::Running;
        op.epoch = Some(epoch);
        op.attempts += 1;
        op.claimed_by = Some(claimed_by.to_string());
        Some(op.clone())
    }

    pub fn due_sessions(&self) -> Vec<SessionId> {
        let inner = self.inner.lock().unwrap();
        let now = Utc::now();
        let mut due: Vec<SessionId> = inner
            .ops
            .iter()
            .filter(|o| o.state == OpState::Queued && o.not_before.is_none_or(|nb| nb <= now))
            .filter(|o| {
                !inner
                    .ops
                    .iter()
                    .any(|r| r.session_id == o.session_id && r.state == OpState::Running)
            })
            .map(|o| o.session_id)
            .collect();
        due.sort_by_key(|s| s.as_uuid());
        due.dedup();
        due
    }

    pub fn record_step(&self, op_id: i64, epoch: i64, step: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner
            .ops
            .iter_mut()
            .find(|o| o.id == op_id && o.epoch == Some(epoch) && o.state == OpState::Running)
        {
            Some(op) => {
                op.step = Some(step.to_string());
                true
            }
            None => false,
        }
    }

    /// Bump only the heartbeat (not the step) — the within-step liveness
    /// beat. Fenced on (op_id, epoch, running). The in-memory op has no
    /// stored `heartbeat_at`, so this just reports whether the row is
    /// still ours (the fence semantics tests rely on).
    pub fn heartbeat(&self, op_id: i64, epoch: i64) -> bool {
        self.inner
            .lock()
            .unwrap()
            .ops
            .iter()
            .any(|o| o.id == op_id && o.epoch == Some(epoch) && o.state == OpState::Running)
    }

    pub fn finish(&self, op_id: i64, epoch: i64, state: OpState, error: Option<&str>) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner
            .ops
            .iter_mut()
            .find(|o| o.id == op_id && o.epoch == Some(epoch) && o.state == OpState::Running)
        {
            Some(op) => {
                op.state = state;
                op.error = error.map(String::from);
                op.finished_at = Some(Utc::now());
                true
            }
            None => false,
        }
    }

    pub fn requeue_with_backoff(
        &self,
        op_id: i64,
        epoch: i64,
        backoff: std::time::Duration,
        error: &str,
    ) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner
            .ops
            .iter_mut()
            .find(|o| o.id == op_id && o.epoch == Some(epoch) && o.state == OpState::Running)
        {
            Some(op) => {
                op.state = OpState::Queued;
                op.not_before =
                    Some(Utc::now() + chrono::Duration::milliseconds(backoff.as_millis() as i64));
                op.error = Some(error.to_string());
                op.epoch = None;
                op.claimed_by = None;
                true
            }
            None => false,
        }
    }

    pub fn cancel_queued(&self, session_id: SessionId, kind: OpKind) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let mut any = false;
        for op in inner
            .ops
            .iter_mut()
            .filter(|o| o.session_id == session_id && o.kind == kind && o.state == OpState::Queued)
        {
            op.state = OpState::Cancelled;
            op.finished_at = Some(Utc::now());
            any = true;
        }
        any
    }

    /// Wake queued ops of `kind` (reset `not_before` to now). Mirrors
    /// `MetadataStore::op_wake_queued_kind` for mock stores. Returns rows
    /// woken.
    pub fn wake_queued_kind(&self, session_id: SessionId, kind: OpKind) -> u64 {
        let now = Utc::now();
        let mut inner = self.inner.lock().unwrap();
        let mut woken = 0;
        for op in inner.ops.iter_mut().filter(|o| {
            o.session_id == session_id
                && o.kind == kind
                && o.state == OpState::Queued
                && o.not_before.map(|nb| nb > now).unwrap_or(false)
        }) {
            op.not_before = Some(now);
            woken += 1;
        }
        woken
    }

    /// A queued-or-running op of `kind` exists for the session — the
    /// duplicate-enqueue guard's read (see
    /// `MetadataStore::op_pending_exists`).
    pub fn pending_exists(&self, session_id: SessionId, kind: OpKind) -> bool {
        self.inner.lock().unwrap().ops.iter().any(|o| {
            o.session_id == session_id
                && o.kind == kind
                && matches!(o.state, OpState::Queued | OpState::Running)
        })
    }

    pub fn cancel_by_id(&self, op_id: i64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner
            .ops
            .iter_mut()
            .find(|o| o.id == op_id && o.state == OpState::Queued)
        {
            Some(op) => {
                op.state = OpState::Cancelled;
                op.finished_at = Some(Utc::now());
                true
            }
            None => false,
        }
    }

    pub fn request_cancel_running(&self, session_id: SessionId, kind: OpKind) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner
            .ops
            .iter_mut()
            .find(|o| o.session_id == session_id && o.kind == kind && o.state == OpState::Running)
        {
            Some(op) => {
                op.payload["_cancel"] = serde_json::Value::Bool(true);
                true
            }
            None => false,
        }
    }

    pub fn cancel_requested(&self, op_id: i64) -> bool {
        self.inner
            .lock()
            .unwrap()
            .ops
            .iter()
            .find(|o| o.id == op_id)
            .map(|o| o.payload.get("_cancel") == Some(&serde_json::Value::Bool(true)))
            .unwrap_or(false)
    }

    pub fn running_for(&self, session_id: SessionId) -> Option<SessionOp> {
        self.inner
            .lock()
            .unwrap()
            .ops
            .iter()
            .find(|o| o.session_id == session_id && o.state == OpState::Running)
            .cloned()
    }

    pub fn get(&self, op_id: i64) -> Option<SessionOp> {
        self.inner
            .lock()
            .unwrap()
            .ops
            .iter()
            .find(|o| o.id == op_id)
            .cloned()
    }

    /// All rows (test assertions).
    pub fn all(&self) -> Vec<SessionOp> {
        self.inner.lock().unwrap().ops.clone()
    }

    /// Test hook: force a RUNNING op's `attempts` and `step` (simulates a
    /// row that has already retried N times and recorded a step — used to
    /// exercise attempt-budget / crash-shortcut fall-through paths).
    pub fn force_attempts_and_step(&self, op_id: i64, attempts: i32, step: Option<&str>) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner.ops.iter_mut().find(|o| o.id == op_id) {
            Some(op) => {
                op.attempts = attempts;
                op.step = step.map(String::from);
                true
            }
            None => false,
        }
    }

    /// Test hook: seed a RUNNING op as if a rival pod had claimed it —
    /// the replacement for the retired session-lease "peer holds the
    /// lease" fixtures.
    pub fn seed_running(&self, session_id: SessionId, kind: OpKind) -> SessionOp {
        match self.enqueue_and_claim(session_id, kind, serde_json::json!({}), None, "rival-pod") {
            EnqueueOutcome::Claimed(op) => op,
            other => panic!("seed_running: session already has ops in flight: {other:?}"),
        }
    }
}
