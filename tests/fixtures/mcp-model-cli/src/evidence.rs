//! Fixed native completion probes. These observations are not execution evidence.
use crate::{digest, emit, Config, Result};
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub fn barrier(root: &Path, env: &str, name: &str) -> Result<Option<PathBuf>> {
    let Some(raw) = std::env::var_os(env) else {
        return Ok(None);
    };
    let path = PathBuf::from(raw);
    if path != root.join(name) {
        return Err("invalid evidence barrier path".into());
    }
    require_absent(&path)?;
    Ok(Some(path))
}
pub fn require_absent(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        _ => Err("evidence release already exists".into()),
    }
}
fn consume_release(path: &Path, owner: u32) -> Result<bool> {
    let before = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
        Ok(metadata) => metadata,
    };
    let valid = |m: &fs::Metadata| {
        m.file_type().is_file()
            && m.len() == 0
            && m.permissions().mode() & 0o7777 == 0o600
            && m.nlink() == 1
            && m.uid() == owner
    };
    if !valid(&before) {
        return Err("invalid evidence release".into());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let opened = file.metadata()?;
    let after = fs::symlink_metadata(path)?;
    if !valid(&opened)
        || !valid(&after)
        || before.ino() != opened.ino()
        || before.dev() != opened.dev()
        || after.ino() != opened.ino()
        || after.dev() != opened.dev()
    {
        return Err("evidence release changed".into());
    }
    fs::remove_file(path)?;
    Ok(true)
}
fn wait_until(
    path: &Path,
    owner: u32,
    input: &mpsc::Receiver<io::Result<String>>,
    deadline: Instant,
) -> Result<Option<Value>> {
    loop {
        // A queued interrupt or native result wins over release.
        match input.try_recv() {
            Ok(line) => return Ok(Some(serde_json::from_str(&line?)?)),
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err("input closed at evidence barrier".into())
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if Instant::now() >= deadline {
            return Err("evidence barrier deadline exceeded".into());
        }
        if consume_release(path, owner)? {
            return Ok(None);
        }
        match input.recv_timeout(Duration::from_millis(20)) {
            Ok(line) => return Ok(Some(serde_json::from_str(&line?)?)),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("input closed at evidence barrier".into())
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}
impl Config {
    pub fn evidence_observation(&mut self, observation: &str, event: Option<&Value>) -> Result<()> {
        let mut proof = json!({"observation":observation,"callId":self.call_id,"tool":self.tool});
        if let Some(plan) = &self.plan {
            proof["planSha256"] = json!(plan.hash);
        }
        if let Some(event) = event {
            proof["eventSha256"] = json!(digest(event)?);
        }
        writeln!(self.proof, "{proof}")?;
        self.proof.flush()?;
        Ok(())
    }
    pub fn prepare_evidence(&self) -> Result<()> {
        if (self.absent || self.premature) && self.mode == "forged" {
            return Err("conflicting evidence mode".into());
        }
        if self.premature && self.premature_barrier.is_none() {
            return Err("premature barrier required".into());
        }
        for path in [&self.completion_barrier, &self.premature_barrier]
            .into_iter()
            .flatten()
        {
            require_absent(path)?;
        }
        Ok(())
    }
    fn evidence_wait(
        &mut self,
        premature: bool,
        input: &mpsc::Receiver<io::Result<String>>,
    ) -> Result<()> {
        let path = if premature {
            &self.premature_barrier
        } else {
            &self.completion_barrier
        };
        let Some(path) = path.clone() else {
            return Ok(());
        };
        self.evidence_observation(
            if premature {
                "prematureBarrierHeld"
            } else {
                "completionBarrierHeld"
            },
            None,
        )?;
        if let Some(value) = wait_until(
            &path,
            fs::metadata(&self.root)?.uid(),
            input,
            Instant::now() + Duration::from_secs(120),
        )? {
            if self.harness == "claude" {
                if value["type"] == "control_request"
                    && value.pointer("/request/subtype") == Some(&json!("interrupt"))
                    && value["request_id"].is_string()
                {
                    emit(
                        json!({"type":"control_response","response":{"subtype":"success","request_id":value["request_id"]}}),
                    )?;
                    emit(
                        json!({"type":"result","subtype":"error_during_execution","session_id":"fixture-session"}),
                    )?;
                }
            } else if value["method"] == "turn/interrupt" && value.get("id").is_some() {
                emit(json!({"id":value["id"],"result":{}}))?;
                emit(
                    json!({"method":"turn/completed","params":{"threadId":"fixture-thread","turn":{"id":"fixture-turn","status":"interrupted"}}}),
                )?;
            }
            return Err("input interrupted evidence barrier".into());
        }
        self.evidence_observation(
            if premature {
                "prematureBarrierReleased"
            } else {
                "completionBarrierReleased"
            },
            None,
        )
    }
    pub fn native_completion(
        &mut self,
        response: &Value,
        input: &mpsc::Receiver<io::Result<String>>,
    ) -> Result<()> {
        let event = if self.harness == "claude" {
            let mut block = json!({"type":"tool_result","tool_use_id":self.call_id});
            if let Some(result) = response.get("result") {
                block["content"] = result
                    .get("content")
                    .ok_or("missing native content")?
                    .clone();
                if let Some(error) = result.get("isError") {
                    block["is_error"] = error.clone();
                }
            } else {
                block["content"] = json!(response
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .ok_or("missing native error message")?);
                block["is_error"] = json!(true);
            }
            json!({"type":"user","message":{"role":"user","content":[block]}})
        } else {
            let result = response.get("result").ok_or("missing native result")?;
            json!({"method":"item/completed","params":{"threadId":"fixture-thread","turnId":"fixture-turn","item":{"id":self.call_id,"type":"dynamicToolCall","tool":self.tool,"status":"completed","success":result["success"],"result":result}}})
        };
        self.evidence_wait(false, input)?;
        emit(event)
    }
    pub fn premature_completion(
        &mut self,
        input: &mpsc::Receiver<io::Result<String>>,
    ) -> Result<()> {
        self.evidence_wait(true, input)?;
        let event = if self.harness == "claude" {
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":self.call_id,"content":[],"is_error":false}]}})
        } else {
            json!({"method":"item/completed","params":{"threadId":"fixture-thread","turnId":"fixture-turn","item":{"id":self.call_id,"type":"dynamicToolCall","tool":self.tool,"status":"completed","success":true}}})
        };
        emit(event.clone())?;
        self.evidence_observation("prematureCompletionEmitted", Some(&event))?;
        self.persist_selection(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expired_deadline_cannot_consume_a_release() {
        let (_tx, rx) = mpsc::channel();
        assert!(wait_until(Path::new("unused-release"), 0, &rx, Instant::now()).is_err());
    }
}
