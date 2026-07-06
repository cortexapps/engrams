//! ADR 0073 phase 2: the coordinator-durable command outbox.
//!
//! Every command down to the guest (a user prompt, an interactive
//! answer) is a `session_outbox` row from the moment the API accepts
//! it until the confirming harness event acks it. The delivery driver
//! (`engram-coordinator::outbox_delivery`) forwards rows oldest-first
//! per session and redelivers on a backoff schedule; the harness's
//! prompt_id dedup (ADR 0052) and idempotent answers (ADR 0054) make
//! redelivery a no-op, so the pipeline is at-least-once end to end
//! with exactly-once effect.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ids::SessionId;

/// What kind of command the row carries. Mirrors the CHECK constraint
/// on `session_outbox.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxKind {
    Prompt,
    Answer,
}

impl OutboxKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            OutboxKind::Prompt => "prompt",
            OutboxKind::Answer => "answer",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "prompt" => Some(OutboxKind::Prompt),
            "answer" => Some(OutboxKind::Answer),
            _ => None,
        }
    }
}

/// One durable command row. `payload` stays a JSON value at this layer
/// (engram-core cannot name harness-proto types — the dependency runs
/// the other way); the coordinator's delivery driver deserializes it
/// into the typed shape at the host-RPC boundary:
/// - kind=prompt: `{"text": String}`
/// - kind=answer: `{"tool_call_id": String, "answers": Answers}`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutboxRow {
    /// Client-minted for prompts (ADR 0052); `answer:<tool_call_id>`
    /// for answers. PRIMARY KEY — a retried enqueue is a no-op.
    pub prompt_id: String,
    pub session_id: SessionId,
    pub kind: OutboxKind,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
    /// Delivery attempts so far (bumped on each relay handoff).
    pub attempts: i32,
    /// Redelivery gate: the driver skips rows with `not_before` in the
    /// future. Set on each delivery to now + ack-timeout, so a
    /// delivered-but-never-acked command re-becomes due by itself.
    pub not_before: DateTime<Utc>,
    /// Last relay handoff (non-authoritative — only `acked_at` is).
    pub delivered_at: Option<DateTime<Utc>>,
    /// Confirming harness event ingested. Terminal.
    pub acked_at: Option<DateTime<Utc>>,
}
