//! Presentation of the trusted runtime's MCP completion envelope.
//!
//! The journal owns outcome and delivery. This module only validates and
//! presents those facts. It does not grant permission or prove an effect.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResultFormat {
    #[default]
    Json,
    Mcp,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct McpToolResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<Source>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<Outcome>,
    delivery: Delivery,
    #[serde(skip_serializing_if = "Option::is_none")]
    output: Option<Output>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<Reason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    denial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result_unavailable: Option<bool>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Source {
    Runtime,
    LocalValidation,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Succeeded,
    Failed,
    #[serde(rename = "outcome_unknown")]
    Unknown,
    Canceled,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Delivery {
    Available,
    Unavailable,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Reason {
    UnsupportedContent,
    ResultLimit,
    OutputSchema,
    UpstreamFailure,
    ExecutionInterrupted,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Output {
    content: Vec<TextContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    structured_content: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum TextContent {
    Text { text: String },
}

impl McpToolResult {
    pub fn parse(raw: &str) -> Result<Self, &'static str> {
        // The normalized payload limit leaves room for the runtime receipt fields.
        if raw.len() > 1024 * 1024 + 1024 {
            return Err("External tool result exceeds the size limit.");
        }
        let document: Value =
            serde_json::from_str(raw).map_err(|_| "Invalid external tool result.")?;
        if document
            .as_object()
            .is_some_and(|object| object.values().any(Value::is_null))
            || document
                .get("output")
                .and_then(Value::as_object)
                .is_some_and(|object| object.values().any(Value::is_null))
        {
            return Err("Invalid external tool result.");
        }
        let result: Self =
            serde_json::from_value(document).map_err(|_| "Invalid external tool result.")?;
        let valid = match (result.delivery, result.output.as_ref()) {
            (Delivery::Available, Some(output)) if result.reason.is_none() => {
                match (result.source, result.outcome) {
                    (None, Some(Outcome::Succeeded)) => {
                        result.denial.is_none() && output.is_error != Some(true)
                    }
                    (None, Some(Outcome::Failed)) => {
                        result.denial.is_none() && output.is_error == Some(true)
                    }
                    (Some(Source::LocalValidation), None) => {
                        result.denial.is_none() && output.is_error == Some(true)
                    }
                    (Some(Source::Runtime), None) => {
                        result.denial.as_ref().is_some_and(|s| !s.is_empty())
                            && output.is_error == Some(true)
                    }
                    _ => false,
                }
            }
            (Delivery::Unavailable, None) => {
                result.reason.is_some()
                    && result.denial.is_none()
                    && match (result.source, result.outcome) {
                        (None, Some(Outcome::Succeeded | Outcome::Failed | Outcome::Unknown)) => {
                            true
                        }
                        (Some(Source::Runtime), Some(Outcome::Canceled | Outcome::Unknown)) => {
                            result.reason == Some(Reason::ExecutionInterrupted)
                        }
                        _ => false,
                    }
            }
            _ => false,
        };
        let unavailable_success =
            result.outcome == Some(Outcome::Succeeded) && result.delivery == Delivery::Unavailable;
        if !valid
            || result
                .result_unavailable
                .is_some_and(|value| value != unavailable_success)
        {
            return Err("Invalid external tool result.");
        }
        Ok(result)
    }

    pub fn success(&self) -> bool {
        self.delivery == Delivery::Available && self.outcome == Some(Outcome::Succeeded)
    }

    fn metadata(&self) -> Value {
        let mut value = serde_json::to_value(self).expect("MCP result is JSON");
        let object = value.as_object_mut().expect("MCP result is an object");
        object.remove("output");
        if let Some(is_error) = self.output.as_ref().and_then(|output| output.is_error) {
            object.insert("isError".into(), Value::Bool(is_error));
        }
        value
    }

    /// MCP has separate tool output and JSON-RPC delivery error surfaces.
    pub fn claude_response(&self, id: Value) -> Value {
        match &self.output {
            Some(output) => {
                let mut result = serde_json::to_value(output).expect("MCP output is JSON");
                result["_meta"] = json!({"engrams/toolResult": self.metadata()});
                json!({"jsonrpc":"2.0", "id":id, "result":result})
            }
            None => json!({"jsonrpc":"2.0", "id":id, "error":{
                "code":-32603, "message":"External tool output is unavailable.", "data":self.metadata()
            }}),
        }
    }

    /// Codex supports text items and one success flag. Keep each original text
    /// item and present the other named fields as separate JSON text items.
    pub fn text_items(&self) -> Vec<String> {
        let mut items = Vec::new();
        if let Some(output) = &self.output {
            for TextContent::Text { text } in &output.content {
                items.push(text.clone());
            }
            if let Some(structured) = &output.structured_content {
                items.push(json!({"structuredContent":structured}).to_string());
            }
        }
        items.push(self.metadata().to_string());
        items
    }

    pub fn codex_response(&self) -> Value {
        let items: Vec<Value> = self
            .text_items()
            .into_iter()
            .map(|text| json!({"type":"inputText","text":text}))
            .collect();
        json!({"success":self.success(), "contentItems":items})
    }
}

pub fn invalid_codex_result() -> Value {
    json!({"success":false,"contentItems":[{"type":"inputText", "text":
        json!({"delivery":"unavailable","reason":"invalid_result"}).to_string()}]})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_binary_resources_and_null_optional_fields() {
        for output in [
            json!({"content":[{"type":"image","data":"AA==","mimeType":"image/png"}]}),
            json!({"content":[{"type":"resource_link","uri":"https://example.test/x"}]}),
            json!({"content":[{"type":"resource","resource":{"uri":"https://example.test/x","text":"x"}}]}),
            json!({"content":[],"structuredContent":null}),
            json!({"content":[],"structuredContent":[]}),
            json!({"content":[],"isError":null}),
        ] {
            let envelope = json!({"outcome":"succeeded","delivery":"available","output":output});
            assert!(
                McpToolResult::parse(&envelope.to_string()).is_err(),
                "accepted {envelope}"
            );
        }
    }

    #[test]
    fn delivery_error_keeps_outcome_and_never_becomes_tool_error() {
        let result = McpToolResult::parse(r#"{"outcome":"succeeded","delivery":"unavailable","reason":"unsupported_content","resultUnavailable":true}"#).unwrap();
        let response = result.claude_response(json!(7));
        assert!(response.get("result").is_none());
        assert_eq!(
            response["error"]["data"],
            json!({"outcome":"succeeded","delivery":"unavailable","reason":"unsupported_content","resultUnavailable":true})
        );
        assert!(!result.success());
    }

    #[test]
    fn local_denials_and_runtime_interruptions_do_not_claim_upstream_failure() {
        for raw in [
            r#"{"source":"local_validation","delivery":"available","output":{"content":[{"type":"text","text":"invalid"}],"isError":true},"resultUnavailable":false}"#,
            r#"{"source":"runtime","denial":"plan_mode","delivery":"available","output":{"content":[{"type":"text","text":"denied"}],"isError":true}}"#,
            r#"{"source":"runtime","outcome":"canceled","delivery":"unavailable","reason":"execution_interrupted","resultUnavailable":false}"#,
        ] {
            let result = McpToolResult::parse(raw).unwrap();
            assert!(!result.success());
            let serialized = serde_json::to_value(&result).unwrap();
            assert_ne!(serialized["outcome"], "failed");
        }
    }

    #[test]
    fn rejects_inconsistent_outcome_and_delivery() {
        for raw in [
            r#"{"outcome":"succeeded","delivery":"available","output":{"content":[],"isError":true}}"#,
            r#"{"outcome":"failed","delivery":"available","output":{"content":[]}}"#,
            r#"{"outcome":"outcome_unknown","delivery":"available","output":{"content":[]}}"#,
            r#"{"outcome":"succeeded","delivery":"unavailable","reason":"result_limit","resultUnavailable":false}"#,
            r#"{"outcome":"succeeded","delivery":"available","output":{"content":[]},"decision":"approve"}"#,
        ] {
            assert!(McpToolResult::parse(raw).is_err(), "accepted {raw}");
        }
    }
}
