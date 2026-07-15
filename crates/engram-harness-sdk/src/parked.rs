use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParkedCallKind {
    DynamicTool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParkedCall {
    pub tool_call_id: String,
    pub kind: ParkedCallKind,
    pub request_id: Value,
    pub tool_name: String,
    pub requested_at: u64,
    pub request_generation: u64,
    pub context: Value,
}

impl ParkedCall {
    pub fn new(
        tool_call_id: impl Into<String>,
        kind: ParkedCallKind,
        request_id: Value,
        tool_name: impl Into<String>,
        request_generation: u64,
        context: Value,
    ) -> Self {
        let requested_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Self {
            tool_call_id: tool_call_id.into(),
            kind,
            request_id,
            tool_name: tool_name.into(),
            requested_at,
            request_generation,
            context,
        }
    }
}

pub struct ParkedCallStore {
    path: PathBuf,
    calls: BTreeMap<String, ParkedCall>,
}

impl ParkedCallStore {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let calls = match std::fs::read(&path) {
            Ok(bytes) => decode_calls(&bytes)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error),
        };
        Ok(Self { path, calls })
    }

    pub fn record(&mut self, call: ParkedCall) -> io::Result<()> {
        let key = call.tool_call_id.clone();
        let previous = self.calls.insert(key.clone(), call);
        if let Err(error) = self.persist() {
            match previous {
                Some(previous) => {
                    self.calls.insert(key, previous);
                }
                None => {
                    self.calls.remove(&key);
                }
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn take(&mut self, tool_call_id: &str) -> io::Result<Option<ParkedCall>> {
        let Some(call) = self.calls.remove(tool_call_id) else {
            return Ok(None);
        };
        if let Err(error) = self.persist() {
            self.calls.insert(tool_call_id.to_string(), call);
            return Err(error);
        }
        Ok(Some(call))
    }

    pub fn all(&self) -> Vec<ParkedCall> {
        self.calls.values().cloned().collect()
    }

    pub fn get(&self, tool_call_id: &str) -> Option<&ParkedCall> {
        self.calls.get(tool_call_id)
    }

    pub fn mark_requests_stale(&mut self) -> io::Result<()> {
        let previous = self.calls.clone();
        for call in self.calls.values_mut() {
            call.request_id = Value::Null;
            call.request_generation = 0;
        }
        if let Err(error) = self.persist() {
            self.calls = previous;
            return Err(error);
        }
        Ok(())
    }

    fn persist(&self) -> io::Result<()> {
        if let Some(parent) = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let body = Value::Array(self.calls.values().map(encode_call).collect());
        let bytes = serde_json::to_vec(&body).map_err(invalid_data)?;
        let mut temp = OsString::from(self.path.as_os_str());
        temp.push(format!(".tmp-{}", std::process::id()));
        let temp = PathBuf::from(temp);
        std::fs::write(&temp, bytes)?;
        match std::fs::rename(&temp, &self.path) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = std::fs::remove_file(temp);
                Err(error)
            }
        }
    }
}

fn encode_call(call: &ParkedCall) -> Value {
    serde_json::json!({
        "tool_call_id": call.tool_call_id,
        "kind": match call.kind {
            ParkedCallKind::DynamicTool => "dynamic_tool",
        },
        "request_id": call.request_id,
        "tool_name": call.tool_name,
        "requested_at": call.requested_at,
        "request_generation": call.request_generation,
        "context": call.context,
    })
}

fn decode_calls(bytes: &[u8]) -> io::Result<BTreeMap<String, ParkedCall>> {
    let value: Value = serde_json::from_slice(bytes).map_err(invalid_data)?;
    let entries = value
        .as_array()
        .ok_or_else(|| invalid_data("parked-call file must contain a JSON array"))?;
    let mut calls = BTreeMap::new();
    for entry in entries {
        let object = entry
            .as_object()
            .ok_or_else(|| invalid_data("parked-call entry must be an object"))?;
        let string = |field: &str| {
            object
                .get(field)
                .and_then(|value| value.as_str())
                .map(str::to_owned)
                .ok_or_else(|| invalid_data(format!("parked-call entry missing {field}")))
        };
        let encoded_kind = string("kind")?;
        if encoded_kind != "dynamic_tool" {
            tracing::warn!(
                kind = %encoded_kind,
                tool_call_id = object
                    .get("tool_call_id")
                    .and_then(|value| value.as_str())
                    .unwrap_or("<missing>"),
                "dropping unsupported parked-call entry during store open"
            );
            continue;
        }
        let kind = ParkedCallKind::DynamicTool;
        let tool_call_id = string("tool_call_id")?;
        let call = ParkedCall {
            tool_call_id: tool_call_id.clone(),
            kind,
            request_id: object.get("request_id").cloned().unwrap_or(Value::Null),
            tool_name: string("tool_name")?,
            requested_at: object
                .get("requested_at")
                .and_then(Value::as_u64)
                .ok_or_else(|| invalid_data("parked-call entry missing requested_at"))?,
            request_generation: object
                .get("request_generation")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            context: object.get("context").cloned().unwrap_or(Value::Null),
        };
        calls.insert(tool_call_id, call);
    }
    Ok(calls)
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("engram-parked-{name}-{}.json", std::process::id()))
    }

    fn call(id: &str) -> ParkedCall {
        ParkedCall::new(
            id,
            ParkedCallKind::DynamicTool,
            json!(41),
            "save_memory",
            7,
            json!({"execution":"sync"}),
        )
    }

    #[test]
    fn parked_calls_round_trip_to_disk_and_take_atomically() {
        let path = test_path("round-trip");
        let _ = std::fs::remove_file(&path);
        let mut store = ParkedCallStore::open(&path).unwrap();
        let expected = call("call-1");

        store.record(expected.clone()).unwrap();
        assert_eq!(store.all(), vec![expected.clone()]);
        assert_eq!(store.take("call-1").unwrap(), Some(expected));
        assert!(store.all().is_empty());
        assert_eq!(
            ParkedCallStore::open(&path)
                .unwrap()
                .take("call-1")
                .unwrap(),
            None
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn parked_calls_survive_store_reopen() {
        let path = test_path("reopen");
        let _ = std::fs::remove_file(&path);
        let expected = call("call-2");
        ParkedCallStore::open(&path)
            .unwrap()
            .record(expected.clone())
            .unwrap();

        assert_eq!(ParkedCallStore::open(&path).unwrap().all(), vec![expected]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn taking_unknown_parked_call_returns_none() {
        let path = test_path("unknown");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            ParkedCallStore::open(&path)
                .unwrap()
                .take("missing")
                .unwrap(),
            None
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn legacy_and_unknown_kinds_are_dropped_without_blocking_store_open() {
        let path = test_path("legacy-kind");
        let _ = std::fs::remove_file(&path);
        std::fs::write(
            &path,
            serde_json::to_vec(&json!([
                {
                    "tool_call_id":"legacy-question",
                    "kind":"user_question",
                    "request_id":41,
                    "tool_name":"requestUserInput",
                    "requested_at":1,
                    "request_generation":1,
                    "context":{}
                },
                {
                    "tool_call_id":"future-call",
                    "kind":"future_kind",
                    "request_id":42,
                    "tool_name":"future_tool",
                    "requested_at":2,
                    "request_generation":1,
                    "context":{}
                },
                {
                    "tool_call_id":"dynamic-call",
                    "kind":"dynamic_tool",
                    "request_id":43,
                    "tool_name":"save_memory",
                    "requested_at":3,
                    "request_generation":1,
                    "context":{"execution":"deferred"}
                }
            ]))
            .unwrap(),
        )
        .unwrap();

        let store = ParkedCallStore::open(&path).expect("legacy rows must not poison store open");
        assert_eq!(
            store
                .all()
                .into_iter()
                .map(|call| call.tool_call_id)
                .collect::<Vec<_>>(),
            vec!["dynamic-call"]
        );
        let _ = std::fs::remove_file(path);
    }
}
