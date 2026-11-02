use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn optional_bool(value: &Value, name: &str) -> Result<Option<bool>> {
    match value.get(name) {
        Some(value) => Ok(Some(value.as_bool().ok_or("invalid boolean")?)),
        None => Ok(None),
    }
}
struct Config {
    tool: String,
    mode: String,
    proof: File,
    barrier: Option<PathBuf>,
}

fn direct_child(root: &Path, path: &Path) -> bool {
    path.parent() == Some(root)
        && matches!(path.file_name(), Some(name) if !name.is_empty() && name != "." && name != "..")
}
fn config() -> Result<Config> {
    let root = PathBuf::from(std::env::var("ENGRAM_FIXTURE_STATE_DIR")?);
    if !root.is_absolute() {
        return Err("state directory must be absolute".into());
    }
    let mut current = PathBuf::new();
    for component in root.components() {
        if !matches!(component, Component::RootDir | Component::Normal(_)) {
            return Err("invalid state directory".into());
        }
        current.push(component);
        if fs::symlink_metadata(&current)?.file_type().is_symlink() {
            return Err("state directory contains a symlink".into());
        }
    }
    let metadata = fs::metadata(&root)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o777 != 0o700 {
        return Err("state directory must be private".into());
    }
    let proof_path = PathBuf::from(std::env::var("ENGRAM_FIXTURE_PROOF")?);
    if !direct_child(&root, &proof_path) {
        return Err("proof must be a direct child of the state directory".into());
    }
    let barrier = std::env::var_os("ENGRAM_FIXTURE_BARRIER").map(PathBuf::from);
    if barrier
        .as_ref()
        .is_some_and(|path| !direct_child(&root, path))
    {
        return Err("barrier must be a direct child of the state directory".into());
    }
    let tool = std::env::var("ENGRAM_FIXTURE_TOOL")?;
    if tool.is_empty()
        || tool.len() > 128
        || !tool.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err("invalid fixture tool".into());
    }
    let mode = std::env::var("ENGRAM_FIXTURE_MODE")?;
    if !["read", "write", "denied", "refire", "held-call", "forged"].contains(&mode.as_str()) {
        return Err("unsupported fixture mode".into());
    }
    let proof = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(proof_path)?;
    Ok(Config {
        tool,
        mode,
        proof,
        barrier,
    })
}
fn stdin_lines() -> mpsc::Receiver<io::Result<String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}
fn wait_barrier(
    path: Option<&Path>,
    input: &mpsc::Receiver<io::Result<String>>,
) -> Result<Option<Value>> {
    let Some(path) = path else { return Ok(None) };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_file() => return Ok(None),
            Ok(_) => return Err("invalid barrier".into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if Instant::now() >= deadline {
            return Err("barrier deadline exceeded".into());
        }
        match input.recv_timeout(Duration::from_millis(20)) {
            Ok(line) => return Ok(Some(serde_json::from_str(&line?)?)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("input closed at barrier".into())
            }
        }
    }
}
fn emit(value: Value) -> Result<()> {
    let mut out = io::stdout().lock();
    writeln!(out, "{value}")?;
    out.flush()?;
    Ok(())
}
fn codex(mut cfg: Config) -> Result<()> {
    if cfg.mode == "forged" {
        return Err("forged mode requires Claude".into());
    }
    let mut initialized = false;
    let mut thread_ready = false;
    let mut pending = false;
    let input = stdin_lines();
    while let Ok(line) = input.recv() {
        let value: Value = serde_json::from_str(&line?)?;
        if value.get("method").is_none() && value.get("id") == Some(&json!(77)) && pending {
            let result = value
                .get("result")
                .and_then(Value::as_object)
                .ok_or("missing dynamic result")?;
            let success = result
                .get("success")
                .and_then(Value::as_bool)
                .ok_or("missing success")?;
            let items = result
                .get("contentItems")
                .and_then(Value::as_array)
                .ok_or("missing content items")?;
            if items.iter().any(|item| {
                item.get("type") != Some(&json!("inputText"))
                    || !item.get("text").is_some_and(Value::is_string)
            }) {
                return Err("unsupported content item".into());
            }
            let text_sentinel = items
                .iter()
                .any(|item| item == &json!({"type":"inputText","text":"sentinel"}));
            let text_sequence = items.first()
                == Some(&json!({"type":"inputText","text":"sentinel"}))
                && items.get(1) == Some(&json!({"type":"inputText","text":"second sentinel"}));
            let parsed: Vec<Value> = items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .filter_map(|text| serde_json::from_str(text).ok())
                .collect();
            let structured = parsed
                .iter()
                .any(|item| item == &json!({"structuredContent":{"fixture":true}}));
            let receipt = parsed
                .iter()
                .find(|item| item.get("delivery").is_some())
                .ok_or("missing receipt")?;
            let delivery = receipt["delivery"]
                .as_str()
                .filter(|s| ["available", "unavailable"].contains(s))
                .ok_or("invalid delivery")?;
            let outcome = receipt.get("outcome").and_then(Value::as_str);
            if outcome.is_some_and(|s| {
                !["succeeded", "failed", "canceled", "outcome_unknown"].contains(&s)
            }) {
                return Err("invalid outcome".into());
            }
            let is_error = optional_bool(receipt, "isError")?;
            let unavailable = optional_bool(receipt, "resultUnavailable")?;
            let denial_plan = receipt.get("denial") == Some(&json!("plan_mode"));
            writeln!(
                cfg.proof,
                "{}",
                json!({"callId":"fixture-call","tool":cfg.tool,"resultReceived":true,"success":success,"textSentinel":text_sentinel,"textSequence":text_sequence,"structuredSentinel":structured,"isError":is_error,"outcome":outcome,"delivery":delivery,"resultUnavailable":unavailable,"denialPlanMode":denial_plan})
            )?;
            cfg.proof.flush()?;
            emit(
                json!({"method":"turn/completed","params":{"threadId":"fixture-thread","turn":{"id":"fixture-turn","status":"completed"}}}),
            )?;
            pending = false;
            continue;
        }
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .ok_or("missing method")?;
        let id = value.get("id").cloned();
        match method {
            "initialized" if initialized && id.is_none() => {}
            "initialize" if !initialized => {
                initialized = true;
                emit(json!({"id":id.ok_or("missing id")?,"result":{}}))?;
            }
            "account/login/start" if initialized => {
                emit(json!({"id":id.ok_or("missing id")?,"result":{}}))?
            }
            "account/read" if initialized => {
                emit(json!({"id":id.ok_or("missing id")?,"result":{"account":{"type":"chatgpt"}}}))?
            }
            "thread/start" | "thread/resume" if initialized && !thread_ready => {
                if method == "thread/resume"
                    && value.pointer("/params/threadId") != Some(&json!("fixture-thread"))
                {
                    return Err("invalid resumed thread".into());
                }
                let tools = value
                    .pointer("/params/dynamicTools")
                    .and_then(Value::as_array)
                    .ok_or("missing dynamic tools")?;
                let selected: Vec<_> = tools
                    .iter()
                    .filter(|tool| tool.get("name") == Some(&json!(cfg.tool)))
                    .collect();
                if selected.len() != 1
                    || !selected[0]["inputSchema"].is_object()
                    || !selected[0]["description"].is_string()
                {
                    return Err("expected tool missing, duplicated, or malformed".into());
                }
                thread_ready = true;
                emit(
                    json!({"id":id.ok_or("missing id")?,"result":{"thread":{"id":"fixture-thread","turns":[]}}}),
                )?;
            }
            "turn/start" if thread_ready && !pending => {
                emit(
                    json!({"id":id.ok_or("missing id")?,"result":{"turn":{"id":"fixture-turn","status":"inProgress"}}}),
                )?;
                if let Some(interrupt) = wait_barrier(cfg.barrier.as_deref(), &input)? {
                    if interrupt["method"] != "turn/interrupt" {
                        return Err("unexpected barrier input".into());
                    }
                    emit(
                        json!({"id":interrupt.get("id").ok_or("missing interrupt id")?,"result":{}}),
                    )?;
                    emit(
                        json!({"method":"turn/completed","params":{"threadId":"fixture-thread","turn":{"id":"fixture-turn","status":"interrupted"}}}),
                    )?;
                    continue;
                }
                emit(
                    json!({"id":77,"method":"item/tool/call","params":{"callId":"fixture-call","tool":cfg.tool,"arguments":{"fixture":cfg.mode},"threadId":"fixture-thread","turnId":"fixture-turn"}}),
                )?;
                pending = true;
            }
            "turn/interrupt" if pending => {
                emit(json!({"id":id.ok_or("missing id")?,"result":{}}))?;
                emit(
                    json!({"method":"turn/completed","params":{"threadId":"fixture-thread","turn":{"id":"fixture-turn","status":"interrupted"}}}),
                )?;
                pending = false;
            }
            _ => return Err("unsupported app-server message".into()),
        }
    }
    if pending {
        return Err("dynamic result missing".into());
    }
    Ok(())
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn read_lines(child: &mut Child) -> Result<mpsc::Receiver<io::Result<String>>> {
    let output = child.stdout.take().ok_or("missing child output")?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    Ok(rx)
}

fn receive(rx: &mpsc::Receiver<io::Result<String>>) -> Result<Value> {
    Ok(serde_json::from_str(
        &rx.recv_timeout(Duration::from_secs(30))??,
    )?)
}

fn interruptible_receive(
    rx: &mpsc::Receiver<io::Result<String>>,
    input: &mpsc::Receiver<io::Result<String>>,
) -> Result<Option<Value>> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(line) => return Ok(Some(serde_json::from_str(&line?)?)),
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("child output closed".into()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        match input.try_recv() {
            Ok(line) => {
                let interrupt: Value = serde_json::from_str(&line?)?;
                if interrupt["type"] != "control_request"
                    || interrupt.pointer("/request/subtype") != Some(&json!("interrupt"))
                {
                    return Err("unexpected turn input".into());
                }
                emit(
                    json!({"type":"control_response","response":{"subtype":"success","request_id":interrupt["request_id"]}}),
                )?;
                emit(
                    json!({"type":"result","subtype":"error_during_execution","session_id":"fixture-session"}),
                )?;
                return Ok(None);
            }
            Err(mpsc::TryRecvError::Disconnected) => return Err("input closed during call".into()),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if Instant::now() >= deadline {
            return Err("child response deadline exceeded".into());
        }
    }
}

fn child_send(child: &mut Child, value: Value) -> Result<()> {
    let input = child.stdin.as_mut().ok_or("missing child input")?;
    writeln!(input, "{value}")?;
    input.flush()?;
    Ok(())
}

fn claude(mut cfg: Config) -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    let argument = |flag: &str| -> Result<PathBuf> {
        let pos = args.iter().position(|a| a == flag).ok_or("missing flag")?;
        Ok(PathBuf::from(
            args.get(pos + 1).ok_or("missing flag value")?,
        ))
    };
    if !args.iter().any(|a| a == "--strict-mcp-config") {
        return Err("missing strict config".into());
    }
    let config: Value = serde_json::from_slice(&fs::read(argument("--mcp-config")?)?)?;
    let servers = config
        .get("mcpServers")
        .and_then(Value::as_object)
        .ok_or("invalid servers")?;
    if servers.len() != 1 {
        return Err("unexpected server count".into());
    }
    let server = servers.get("engrams").ok_or("missing self bridge")?;
    if server["type"] != "stdio" || server["args"] != json!(["mcp-bridge"]) {
        return Err("invalid bridge config".into());
    }
    let command = server["command"].as_str().ok_or("missing bridge command")?;
    if !Path::new(command).is_absolute() {
        return Err("bridge command must be absolute".into());
    }
    let settings: Value = serde_json::from_slice(&fs::read(argument("--settings")?)?)?;
    let hook = settings
        .pointer("/hooks/PreToolUse/0/hooks/0/command")
        .and_then(Value::as_str)
        .ok_or("missing hook")?;
    if hook.strip_suffix(" hook-bridge") != Some(command) {
        return Err("invalid hook command".into());
    }
    let mut bridge = ChildGuard(
        Command::new(command)
            .arg("mcp-bridge")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?,
    );
    let bridge_rx = read_lines(&mut bridge.0)?;
    child_send(
        &mut bridge.0,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}),
    )?;
    if receive(&bridge_rx)?["id"] != 1 {
        return Err("invalid initialize response".into());
    }
    child_send(
        &mut bridge.0,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )?;
    child_send(
        &mut bridge.0,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    )?;
    let list = receive(&bridge_rx)?;
    let tools = list
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .ok_or("missing tools")?;
    let selected: Vec<_> = tools
        .iter()
        .filter(|tool| tool["name"] == cfg.tool)
        .collect();
    if selected.len() != 1 || !selected[0]["inputSchema"].is_object() {
        return Err("expected tool missing, duplicated, or malformed".into());
    }
    emit(json!({"type":"system","subtype":"init","session_id":"fixture-session"}))?;
    let input = stdin_lines();
    let mut call_request_id = 3_u64;
    while let Ok(line) = input.recv() {
        let prompt: Value = serde_json::from_str(&line?)?;
        if prompt["type"] != "user" {
            return Err("unsupported Claude input".into());
        }
        let mut hook = ChildGuard(
            Command::new(command)
                .arg("hook-bridge")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?,
        );
        let hook_rx = read_lines(&mut hook.0)?;
        let arguments = json!({"fixture":cfg.mode});
        let call_tool = if cfg.mode == "forged" {
            "fixture_forged"
        } else {
            &cfg.tool
        };
        child_send(
            &mut hook.0,
            json!({"tool_name":format!("mcp__engrams__{}",call_tool),"tool_use_id":"fixture-call","tool_input":arguments,"permission_mode":"default"}),
        )?;
        drop(hook.0.stdin.take());
        let Some(decision) = interruptible_receive(&hook_rx, &input)? else {
            continue;
        };
        match decision
            .pointer("/hookSpecificOutput/permissionDecision")
            .and_then(Value::as_str)
        {
            Some("allow") => {}
            Some("defer") | Some("deny") => {
                emit(
                    json!({"type":"result","subtype":"success","session_id":"fixture-session","result":"fixture"}),
                )?;
                continue;
            }
            _ => return Err("invalid hook decision".into()),
        }
        if let Some(interrupt) = wait_barrier(cfg.barrier.as_deref(), &input)? {
            if interrupt["type"] != "control_request"
                || interrupt.pointer("/request/subtype") != Some(&json!("interrupt"))
            {
                return Err("unexpected barrier input".into());
            }
            emit(
                json!({"type":"control_response","response":{"subtype":"success","request_id":interrupt["request_id"]}}),
            )?;
            emit(
                json!({"type":"result","subtype":"error_during_execution","session_id":"fixture-session"}),
            )?;
            continue;
        }
        child_send(
            &mut bridge.0,
            json!({"jsonrpc":"2.0","id":call_request_id,"method":"tools/call","params":{"name":call_tool,"arguments":arguments,"_meta":{"claudecode/toolUseId":"fixture-call"}}}),
        )?;
        let Some(response) = interruptible_receive(&bridge_rx, &input)? else {
            child_send(
                &mut bridge.0,
                json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":call_request_id}}),
            )?;
            call_request_id += 1;
            continue;
        };
        if response["id"] != call_request_id {
            return Err("invalid call response".into());
        }
        if cfg.mode == "forged" {
            if response.pointer("/error/code") != Some(&json!(-32602)) {
                return Err("forged tool was not rejected".into());
            }
            writeln!(
                cfg.proof,
                "{}",
                json!({"callId":"fixture-call","tool":"fixture_forged","forgedRejected":true})
            )?;
            cfg.proof.flush()?;
            emit(
                json!({"type":"result","subtype":"success","session_id":"fixture-session","result":"fixture"}),
            )?;
            break;
        }
        let result = response.get("result");
        let receipt = response
            .pointer("/result/_meta/engrams~1toolResult")
            .or_else(|| response.pointer("/error/data"))
            .ok_or("missing receipt")?;
        let delivery = receipt["delivery"]
            .as_str()
            .filter(|s| ["available", "unavailable"].contains(s))
            .ok_or("invalid delivery")?;
        let outcome = receipt.get("outcome").and_then(Value::as_str);
        if outcome
            .is_some_and(|s| !["succeeded", "failed", "canceled", "outcome_unknown"].contains(&s))
        {
            return Err("invalid outcome".into());
        }
        let text = result
            .and_then(|v| v.get("content"))
            .and_then(Value::as_array)
            .is_some_and(|content| {
                content
                    .iter()
                    .any(|item| item == &json!({"type":"text","text":"sentinel"}))
            });
        let text_sequence = result
            .and_then(|v| v.get("content"))
            .and_then(Value::as_array)
            .is_some_and(|content| {
                content
                    == &vec![
                        json!({"type":"text","text":"sentinel"}),
                        json!({"type":"text","text":"second sentinel"}),
                    ]
            });
        let structured =
            result.and_then(|v| v.get("structuredContent")) == Some(&json!({"fixture":true}));
        if (delivery == "available") != result.is_some() {
            return Err("inconsistent delivery response".into());
        }
        if let Some(value) = result {
            if !value.get("content").is_some_and(Value::is_array) {
                return Err("missing content".into());
            }
        }
        let is_error = result
            .map(|v| optional_bool(v, "isError"))
            .transpose()?
            .flatten();
        let unavailable = optional_bool(receipt, "resultUnavailable")?;
        let denial_plan = receipt.get("denial") == Some(&json!("plan_mode"));
        writeln!(
            cfg.proof,
            "{}",
            json!({"callId":"fixture-call","tool":cfg.tool,"resultReceived":true,"textSentinel":text,"textSequence":text_sequence,"structuredSentinel":structured,"isError":is_error,"outcome":outcome,"delivery":delivery,"resultUnavailable":unavailable,"denialPlanMode":denial_plan})
        )?;
        cfg.proof.flush()?;
        emit(
            json!({"type":"result","subtype":"success","session_id":"fixture-session","result":"fixture"}),
        )?;
        break;
    }
    Ok(())
}
fn main() {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--fixture-version")) {
        println!("engram-mcp-model-fixture 1");
        return;
    }
    let result = (|| -> Result<()> {
        let cfg = config()?;
        if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("app-server")) {
            codex(cfg)
        } else {
            claude(cfg)
        }
    })();
    if result.is_err() {
        eprintln!("fixture protocol failure");
        std::process::exit(1);
    }
}
