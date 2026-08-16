//! Time-to-first-output: `RunStarted` → the turn's first observable
//! output, logged once per turn.
//!
//! This window is the largest unmeasured term in cold-boot latency.
//! Everything before it is host-side and instrumented
//! (`engram_sandbox_boot_seconds`, `engram_prompt_to_run_started_seconds`);
//! everything inside it is opaque agent-CLI init plus the first model
//! round trip. A ~60s stale-ARP black-hole sat in exactly this gap
//! undetected (2026-08-16) because no signal spanned it — localising it
//! took a hand-built prod repro rather than a metric.
//!
//! It lives in the SDK rather than in an adapter because every harness
//! reaches the host through `serve`'s event pump, so one observer here
//! covers claude, codex, noop and anything added later, and the
//! definition of "first output" stays identical across them (an
//! adapter-local timer would drift per agent and make the numbers
//! incomparable).
//!
//! **First output, not first token.** Only streaming agents emit
//! `AgentMessageChunk`; codex and noop surface a turn's first sign of
//! life as a complete `AgentMessage`, a tool call, or a `Generation`.
//! Taking the first of ANY of those keeps the measurement meaningful
//! for every adapter — for a token-streaming agent it IS the first
//! token, and for the others it is the earliest moment the turn became
//! visible.

use std::time::Instant;

use engram_harness_proto::{AgentRole, HarnessEvent};

/// Per-turn stopwatch. One in-flight turn at a time (the engine loops
/// serially over prompts), so a single slot is enough; a `RunStarted`
/// always supersedes whatever came before rather than accumulating.
#[derive(Debug, Default)]
pub(crate) struct FirstOutput {
    pending: Option<(String, Instant)>,
}

impl FirstOutput {
    /// Observe one event on its way to the host.
    ///
    /// Call this ONLY where an event is first pulled off the engine
    /// channel. `pump_events` re-sends a held event after a reconnect,
    /// and observing that path too would restart the clock mid-turn.
    pub(crate) fn observe(&mut self, event: &HarnessEvent) {
        if let HarnessEvent::RunStarted { run_id, .. } = event {
            self.pending = Some((run_id.clone(), Instant::now()));
            return;
        }
        let Some(run_id) = output_run_id(event) else {
            return;
        };
        // Only the run we're timing. A late output attributed to a
        // previous run must not consume this turn's stopwatch.
        let Some((pending_run, started)) = self.pending.as_ref() else {
            return;
        };
        if pending_run != run_id {
            return;
        }
        tracing::info!(
            run_id = %run_id,
            ttfo_ms = started.elapsed().as_millis() as u64,
            "first output"
        );
        self.pending = None;
    }
}

/// The run this event is the first *output* of, if it is one.
///
/// Deliberately exhaustive with no wildcard arm: a new `HarnessEvent`
/// variant should force a decision about whether it counts as output,
/// not silently default to "no" and quietly skew the measurement.
fn output_run_id(event: &HarnessEvent) -> Option<&str> {
    match event {
        // The model produced something the user can see.
        HarnessEvent::AgentMessageChunk { run_id, .. } => Some(run_id),
        HarnessEvent::AgentMessage { run_id, role, .. } => match role {
            // A user/system message on the event stream is an echo or a
            // synthetic turn, not the model answering.
            AgentRole::Assistant => Some(run_id),
            AgentRole::User | AgentRole::System => None,
        },
        // The model decided to act. For a tool-first turn this is the
        // real first output — waiting for prose would overstate it.
        HarnessEvent::ToolCallRequested { run_id, .. } => Some(run_id),
        HarnessEvent::ToolCallStarted { run_id, .. } => Some(run_id),
        // Accounting for a completed model call: proof a round trip
        // finished even when an adapter emits no text.
        HarnessEvent::Generation { run_id, .. } => Some(run_id),

        // Not output: lifecycle, queue bookkeeping, side effects, and
        // events that trail the first output rather than being it.
        HarnessEvent::RunStarted { .. }
        | HarnessEvent::ToolCallCompleted { .. }
        | HarnessEvent::RunCompleted { .. }
        | HarnessEvent::RunInterrupted { .. }
        | HarnessEvent::Idle
        | HarnessEvent::PromptQueued { .. }
        | HarnessEvent::PromptEdited { .. }
        | HarnessEvent::PromptDequeued { .. }
        | HarnessEvent::FileChanged { .. }
        | HarnessEvent::TitleSuggested { .. }
        | HarnessEvent::PromptSteered { .. }
        | HarnessEvent::Parked
        | HarnessEvent::BrowserActivity { .. }
        | HarnessEvent::Busy
        | HarnessEvent::RunCost { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_started(run: &str) -> HarnessEvent {
        HarnessEvent::RunStarted {
            run_id: run.into(),
            prompt_id: None,
            prompt_summary: None,
        }
    }

    fn chunk(run: &str) -> HarnessEvent {
        HarnessEvent::AgentMessageChunk {
            run_id: run.into(),
            message_id: "m1".into(),
            chunk: "hi".into(),
        }
    }

    fn message(run: &str, role: AgentRole) -> HarnessEvent {
        HarnessEvent::AgentMessage {
            run_id: run.into(),
            message_id: "m1".into(),
            role,
            text: "hi".into(),
        }
    }

    #[test]
    fn times_the_first_output_then_stops() {
        let mut f = FirstOutput::default();
        f.observe(&run_started("r1"));
        assert!(f.pending.is_some());
        f.observe(&chunk("r1"));
        assert!(f.pending.is_none(), "first output must consume the timer");
        // Later output in the same turn must not re-log.
        f.observe(&chunk("r1"));
        assert!(f.pending.is_none());
    }

    /// Codex and noop never stream chunks; their turns must still be
    /// measured off whatever they do emit.
    #[test]
    fn a_complete_assistant_message_counts_as_output() {
        let mut f = FirstOutput::default();
        f.observe(&run_started("r1"));
        f.observe(&message("r1", AgentRole::Assistant));
        assert!(f.pending.is_none());
    }

    #[test]
    fn a_tool_call_counts_as_output() {
        let mut f = FirstOutput::default();
        f.observe(&run_started("r1"));
        f.observe(&HarnessEvent::ToolCallStarted {
            run_id: "r1".into(),
            tool_call_id: "t1".into(),
            tool_name: "Bash".into(),
            args_summary: Some("ls".into()),
        });
        assert!(f.pending.is_none(), "a tool-first turn has produced output");
    }

    #[test]
    fn non_assistant_messages_are_not_output() {
        let mut f = FirstOutput::default();
        f.observe(&run_started("r1"));
        f.observe(&message("r1", AgentRole::User));
        f.observe(&message("r1", AgentRole::System));
        assert!(
            f.pending.is_some(),
            "an echoed user/system message is not the model answering"
        );
    }

    #[test]
    fn output_from_another_run_does_not_consume_the_timer() {
        let mut f = FirstOutput::default();
        f.observe(&run_started("r2"));
        f.observe(&chunk("r1"));
        assert!(
            f.pending.is_some(),
            "a straggler from the previous run must not stop this turn's clock"
        );
    }

    #[test]
    fn a_new_run_supersedes_an_unfinished_one() {
        let mut f = FirstOutput::default();
        f.observe(&run_started("r1"));
        f.observe(&run_started("r2"));
        f.observe(&chunk("r1"));
        assert!(f.pending.is_some(), "r1's chunk is stale once r2 started");
        f.observe(&chunk("r2"));
        assert!(f.pending.is_none());
    }

    #[test]
    fn output_before_any_run_started_is_ignored() {
        let mut f = FirstOutput::default();
        f.observe(&chunk("r1"));
        assert!(f.pending.is_none());
    }
}
