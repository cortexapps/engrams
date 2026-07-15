//! Canonical question payloads used only at agent-native binding boundaries.
//!
//! ADR 0089 P5d removed these shapes from the positional harness wire. They
//! remain useful inside the Claude and Codex adapters while translating their
//! built-in question tools to generic `args_json` / `result_json` frames.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub type Answers = BTreeMap<String, Vec<String>>;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Question {
    pub question: String,
    pub header: String,
    #[serde(rename = "multiSelect")]
    pub multi_select: bool,
    pub options: Vec<QuestionOption>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}
