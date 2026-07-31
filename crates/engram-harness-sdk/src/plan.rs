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
}
