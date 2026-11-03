use crate::scalar::{digest, parse_object};
use crate::Result;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

#[derive(Clone)]
pub struct Step {
    pub marker: String,
    pub tool: String,
    pub arguments: Value,
    pub call_id: String,
    pub native_json: bool,
    pub expected: Option<Value>,
    pub absent: bool,
    pub premature: bool,
}
pub struct Plan {
    pub hash: String,
    steps: Vec<Step>,
}
pub fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
pub fn tool_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
fn exact_keys(value: &Value, required: &[&str], optional: &[&str]) -> Result<()> {
    let object = value.as_object().ok_or("invalid object")?;
    if required.iter().any(|key| !object.contains_key(*key))
        || object
            .keys()
            .any(|key| !required.contains(&key.as_str()) && !optional.contains(&key.as_str()))
    {
        return Err("invalid object fields".into());
    }
    Ok(())
}
fn text(value: &Value, key: &str) -> Result<String> {
    Ok(value
        .get(key)
        .and_then(Value::as_str)
        .ok_or("missing string")?
        .to_owned())
}
impl Plan {
    pub fn from_env() -> Result<Option<Self>> {
        let raw = match std::env::var("ENGRAM_FIXTURE_CALL_PLAN_JSON") {
            Ok(raw) => raw,
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(_) => return Err("invalid plan environment".into()),
        };
        for key in [
            "ENGRAM_FIXTURE_TOOL",
            "ENGRAM_FIXTURE_ARGUMENTS_JSON",
            "ENGRAM_FIXTURE_CALL_ID",
            "ENGRAM_FIXTURE_EXPECTED_RESULT_JSON",
        ] {
            if std::env::var_os(key).is_some() {
                return Err("plan conflicts with scalar configuration".into());
            }
        }
        // The root object, steps array and step object enclose each field.
        // Preserve the expected-result depth bound through those three levels.
        let value = parse_object(&raw, 65536, 128 + 3)?;
        exact_keys(&value, &["steps"], &[])?;
        let steps = value["steps"].as_array().ok_or("invalid plan steps")?;
        if steps.is_empty() || steps.len() > 16 {
            return Err("invalid plan length".into());
        }
        let mut markers = BTreeSet::new();
        let mut parsed: Vec<Step> = Vec::new();
        for step in steps {
            exact_keys(
                step,
                &["marker", "tool", "arguments", "callId", "resultFormat"],
                &["expectedResult", "expectExposure", "completionProjection"],
            )?;
            let marker = text(step, "marker")?;
            let call_id = text(step, "callId")?;
            let tool = text(step, "tool")?;
            if !identifier(&marker)
                || !identifier(&call_id)
                || !tool_name(&tool)
                || !markers.insert(marker.clone())
            {
                return Err("invalid or duplicate plan identity".into());
            }
            let arguments = parse_object(&step["arguments"].to_string(), 4096, 16)?;
            let expected = step
                .get("expectedResult")
                .map(|value| parse_object(&value.to_string(), 1024 * 1024, 128))
                .transpose()?;
            let native_json = match step["resultFormat"].as_str() {
                Some("json") => true,
                Some("mcp") => false,
                _ => return Err("invalid plan result format".into()),
            };
            let absent = match step.get("expectExposure").and_then(Value::as_str) {
                Some("absent") => true,
                Some("present") => false,
                None if step.get("expectExposure").is_none() => false,
                _ => return Err("invalid exposure expectation".into()),
            };
            let premature = match step.get("completionProjection").and_then(Value::as_str) {
                Some("premature") => true,
                None if step.get("completionProjection").is_none() => false,
                _ => return Err("invalid completion projection".into()),
            };
            if (absent || premature)
                && (native_json || expected.is_some() || !tool.starts_with("mcp_"))
                || absent && premature
            {
                return Err("invalid evidence step".into());
            }
            let duplicates: Vec<_> = parsed
                .iter()
                .filter(|prior| prior.call_id == call_id)
                .collect();
            if !duplicates.is_empty()
                && (duplicates.len() != 1
                    || !absent
                    || duplicates[0].absent
                    || duplicates[0].premature
                    || duplicates[0].native_json
                    || duplicates[0].tool != tool
                    || digest(&duplicates[0].arguments)? != digest(&arguments)?)
            {
                return Err("invalid repeated call identity".into());
            }
            parsed.push(Step {
                marker,
                tool,
                arguments,
                call_id,
                native_json,
                expected,
                absent,
                premature,
            });
        }
        Ok(Some(Self {
            hash: digest(&value)?,
            steps: parsed,
        }))
    }
    pub fn select(&self, prompt: &str) -> Result<Step> {
        if prompt.len() > 65536 {
            return Err("prompt exceeds fixture limit".into());
        }
        let mut tokens = prompt.split("[[fixture:").skip(1);
        let token = tokens.next().ok_or("missing plan marker")?;
        if tokens.next().is_some() {
            return Err("ambiguous plan marker".into());
        }
        let marker = token.split_once("]]").ok_or("invalid plan marker")?.0;
        self.step(marker)
    }
    fn step(&self, marker: &str) -> Result<Step> {
        self.steps
            .iter()
            .find(|step| step.marker == marker)
            .cloned()
            .ok_or_else(|| "unknown plan marker".into())
    }
    fn read_receipt(&self, root: &Path, harness: &str, mode: &str) -> Result<Option<Value>> {
        let path = root.join("plan-selection.json");
        let metadata = match fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_file() || metadata.len() > 4096 {
            return Err("invalid plan receipt".into());
        }
        let mut raw = String::new();
        fs::File::open(path)?.take(4097).read_to_string(&mut raw)?;
        let value = parse_object(&raw, 4096, 4)?;
        exact_keys(
            &value,
            &["planSha256", "harness", "mode", "marker", "pending"],
            &[],
        )?;
        if value["planSha256"] != self.hash
            || value["harness"] != harness
            || value["mode"] != mode
            || !value["pending"].is_boolean()
        {
            return Err("mismatched plan receipt".into());
        }
        self.step(value["marker"].as_str().ok_or("invalid receipt marker")?)?;
        Ok(Some(value))
    }
    pub fn resume(&self, root: &Path, harness: &str, mode: &str) -> Result<Step> {
        let value = self
            .read_receipt(root, harness, mode)?
            .ok_or("missing pending plan receipt")?;
        if value["pending"] != true {
            return Err("plan selection already completed".into());
        }
        self.step(value["marker"].as_str().ok_or("invalid receipt marker")?)
    }
    pub fn persist(
        &self,
        root: &Path,
        harness: &str,
        mode: &str,
        marker: &str,
        pending: bool,
    ) -> Result<()> {
        self.read_receipt(root, harness, mode)?;
        let temporary = root.join(format!("plan-selection-{}.tmp", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        let result = (|| -> Result<()> {
            write!(
                file,
                "{}",
                json!({"planSha256":self.hash,"harness":harness,"mode":mode,"marker":marker,"pending":pending})
            )?;
            file.sync_all()?;
            fs::rename(&temporary, root.join("plan-selection.json"))?;
            fs::File::open(root)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

pub fn prompt_text(value: &Value, claude: bool) -> Result<String> {
    if claude && value.pointer("/message/role").and_then(Value::as_str) != Some("user") {
        return Err("not a current user prompt".into());
    }
    let content = if claude {
        value.pointer("/message/content")
    } else {
        value.pointer("/params/input")
    }
    .ok_or("missing current prompt")?;
    if claude {
        if let Some(text) = content.as_str() {
            return Ok(text.to_owned());
        }
    }
    let items = content.as_array().ok_or("invalid current prompt")?;
    let mut parts = Vec::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("text") {
            return Err("nontext current prompt".into());
        }
        parts.push(
            item.get("text")
                .and_then(Value::as_str)
                .ok_or("invalid prompt text")?,
        );
    }
    Ok(parts.join("\n"))
}

pub fn exposed_mcp_hash(tools: &[Value]) -> Result<String> {
    let mut names = BTreeSet::new();
    for tool in tools {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .ok_or("invalid exposed tool name")?;
        if name.starts_with("mcp_")
            && (!tool_name(name) || !names.insert(name) || names.len() > 128)
        {
            return Err("invalid exposed MCP tool set".into());
        }
    }
    digest(&json!(names.into_iter().collect::<Vec<_>>()))
}
