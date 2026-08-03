//! The `exit_plan_mode` decision payload (ADR 0107), shared by every
//! adapter. The orchestrator zod-validates
//! `{decision: "approve"|"reject", feedback?}` before it reaches the wire;
//! this parse re-checks tolerantly so an unrecognized shape falls back to
//! each adapter's generic result path instead of misclassifying.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct PlanDecision {
    pub decision: String,
    #[serde(default)]
    pub feedback: Option<String>,
}

impl PlanDecision {
    pub fn approved(&self) -> bool {
        self.decision == "approve"
    }

    /// The reason a rejected plan is surfaced to the model with — the
    /// reviewer's feedback, or a generic revision ask.
    pub fn reject_reason(&self) -> String {
        match self.feedback.as_deref().filter(|f| !f.trim().is_empty()) {
            Some(feedback) => format!("Plan rejected by the reviewer: {feedback}"),
            None => {
                "Plan rejected by the reviewer. Revise the plan and present it again.".to_string()
            }
        }
    }
}

/// What a rejected plan must say to the model, on EVERY delivery path: the
/// claude hook's deny verdict, claude's abandoned-re-fire fallback message, and
/// codex's revision turn.
///
/// A verdict alone is not enough. "Plan rejected by the reviewer: <feedback>"
/// got the model to re-propose a byte-identical plan (session 93869a67), and on
/// codex the same shape ended the turn outright (fe3cd981, 98111e00). The
/// imperative — revise, call the tool again, stay read-only — is load-bearing.
pub fn changes_requested_message(decision: &PlanDecision) -> String {
    format!(
        "{} Revise the plan now and call exit_plan_mode again with the updated \
         markdown. Stay in plan mode: do not modify files.",
        decision.reject_reason()
    )
}

/// What an APPROVED plan says to the model. Delivered as the `exit_plan_mode`
/// tool result on the id-stable re-fire (the injected tool re-fires like any
/// deferred MCP tool — unlike the old native binding), so it must be an
/// imperative to build, not a bare "approved". Symmetric with
/// `changes_requested_message`.
pub const APPROVED_MESSAGE: &str =
    "Your plan was approved. Implement it now, following the plan you presented.";

/// Render an `exit_plan_mode` decision as the instruction the model reads, on
/// every delivery path (the claude bridge serve on re-fire, the tier-2 fallback
/// user message). Approve → build; reject → revise.
pub fn decision_message(decision: &PlanDecision) -> String {
    if decision.approved() {
        APPROVED_MESSAGE.to_string()
    } else {
        changes_requested_message(decision)
    }
}

pub fn parse_plan_decision(result_json: &str) -> Option<PlanDecision> {
    serde_json::from_str::<PlanDecision>(result_json)
        .ok()
        .filter(|d| d.decision == "approve" || d.decision == "reject")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_decisions_and_rejects_garbage() {
        assert!(parse_plan_decision(r#"{"decision":"approve"}"#)
            .unwrap()
            .approved());
        let reject =
            parse_plan_decision(r#"{"decision":"reject","feedback":"add tests"}"#).unwrap();
        assert!(!reject.approved());
        assert!(reject.reject_reason().contains("add tests"));
        assert!(parse_plan_decision(r#"{"decision":"maybe"}"#).is_none());
        assert!(parse_plan_decision(r#"{"answers":{}}"#).is_none());
        assert!(parse_plan_decision("not json").is_none());
    }

    #[test]
    fn empty_feedback_gets_the_generic_ask() {
        let reject = parse_plan_decision(r#"{"decision":"reject","feedback":"  "}"#).unwrap();
        assert!(reject.reject_reason().contains("Revise the plan"));
    }

    #[test]
    fn decision_message_is_build_for_approve_and_revise_for_reject() {
        let approve = parse_plan_decision(r#"{"decision":"approve"}"#).unwrap();
        let msg = decision_message(&approve);
        assert_eq!(msg, APPROVED_MESSAGE);
        assert!(msg.contains("Implement it now"), "approve → build: {msg}");

        let reject =
            parse_plan_decision(r#"{"decision":"reject","feedback":"add tests"}"#).unwrap();
        let msg = decision_message(&reject);
        assert!(msg.contains("add tests"));
        assert!(
            msg.contains("call exit_plan_mode again"),
            "reject → revise: {msg}"
        );
    }
}
