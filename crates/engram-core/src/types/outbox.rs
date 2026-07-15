//! ADR 0073 phase 2: the coordinator-durable command outbox.
//!
//! Every command down to the guest (a user prompt or generic tool result)
//! is a `session_outbox` row from the moment the API accepts
//! it until the confirming harness event acks it. The delivery driver
//! (`engram-coordinator::outbox_delivery`) forwards rows oldest-first
//! per session and redelivers on a backoff schedule; the harness's
//! prompt_id and tool-call dedup make
//! redelivery a no-op, so the pipeline is at-least-once end to end
//! with exactly-once effect.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::ids::SessionId;

/// Globally unique durable identity for one session-scoped tool result.
/// Tool-call ids are minted by untrusted harnesses and are not unique across
/// sessions, while `session_outbox.prompt_id` is a global primary key.
pub fn tool_result_outbox_id(session_id: SessionId, tool_call_id: &str) -> String {
    format!("tool_result:{session_id}:{tool_call_id}")
}

/// What kind of command the row carries. Mirrors the CHECK constraint
/// on `session_outbox.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxKind {
    Prompt,
    /// ADR 0089 P5d parse tombstone only. Applied migrations and real
    /// databases still admit pre-flag-day rows; the coordinator retires
    /// them with a warning and never forwards them to the guest.
    Answer,
    ToolResult,
}

impl OutboxKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            OutboxKind::Prompt => "prompt",
            OutboxKind::Answer => "answer",
            OutboxKind::ToolResult => "tool_result",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "prompt" => Some(OutboxKind::Prompt),
            "answer" => Some(OutboxKind::Answer),
            "tool_result" => Some(OutboxKind::ToolResult),
            _ => None,
        }
    }
}

/// One durable command row. `payload` stays a JSON value at this layer
/// (engram-core cannot name harness-proto types — the dependency runs
/// the other way); the coordinator's delivery driver deserializes it
/// into the typed shape at the host-RPC boundary:
/// - kind=prompt: `{"text": String}`
/// - kind=answer: legacy parse tombstone; never forwarded
/// - kind=tool_result: `{"tool_call_id": String, "result_json": String}`
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutboxRow {
    /// Client-minted for prompts (ADR 0052); legacy rows may contain
    /// `answer:<tool_call_id>`;
    /// `tool_result:<session_id>:<tool_call_id>` identifies generic tool
    /// results without trusting call ids to be globally unique.
    /// PRIMARY KEY — a retried enqueue is a no-op.
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

#[cfg(test)]
mod tests {
    use super::{tool_result_outbox_id, OutboxKind};
    use crate::SessionId;

    #[test]
    fn outbox_kind_strings_round_trip() {
        for (kind, encoded) in [
            (OutboxKind::Prompt, "prompt"),
            (OutboxKind::Answer, "answer"),
            (OutboxKind::ToolResult, "tool_result"),
        ] {
            assert_eq!(kind.as_str(), encoded);
            assert_eq!(OutboxKind::parse(encoded), Some(kind));
        }
        assert_eq!(OutboxKind::parse("unknown"), None);
    }

    #[test]
    fn tool_result_ids_are_namespaced_by_session() {
        let first = SessionId::new();
        let second = SessionId::new();
        assert_ne!(
            tool_result_outbox_id(first, "call-1"),
            tool_result_outbox_id(second, "call-1")
        );
        assert_eq!(
            tool_result_outbox_id(first, "call-1"),
            format!("tool_result:{first}:call-1")
        );
    }
}
