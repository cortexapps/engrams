mod evidence;
mod plan;
mod scalar;
use scalar::{configured_object, digest};
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
    completion_barrier: Option<PathBuf>,
    premature_barrier: Option<PathBuf>,
    absent: bool,
    premature: bool,
    arguments: Value,
    call_id: String,
    expected: Option<Value>,
    extended_proof: bool,
    selected_schema: Option<String>,
    exposed_hash: Option<String>,
    exposed_tools: Vec<Value>,
    plan: Option<plan::Plan>,
    root: PathBuf,
    marker: Option<String>,
    native_json: bool,
    harness: &'static str,
}

fn direct_child(root: &Path, path: &Path) -> bool {
    path.parent() == Some(root)
        && matches!(path.file_name(), Some(name) if !name.is_empty() && name != "." && name != "..")
}
fn proof_file<R: io::Read>(
    root: &Path,
    path: &Path,
    pid: u32,
    entropy: impl FnOnce() -> io::Result<R>,
) -> Result<File> {
    if !direct_child(root, path) {
        return Err("proof must be a direct child of the state directory".into());
    }
    let basename = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("invalid proof name")?;
    let token = ["{launch}", "{pid}"]
        .into_iter()
        .find(|token| basename.contains(token));
    let literal = token.map_or_else(
        || basename.to_owned(),
        |token| basename.replacen(token, "", 1),
    );
    if literal.contains(['{', '}']) {
        return Err("invalid proof token".into());
    }
    // Launch-local observation only, never a session, call, or run identity.
    let basename = match token {
        Some("{launch}") => {
            let mut bytes = [0; 16];
            entropy()?.read_exact(&mut bytes)?;
            basename.replace(
                "{launch}",
                &format!("{pid}-{:032x}", u128::from_be_bytes(bytes)),
            )
        }
        Some(_) => basename.replace("{pid}", &pid.to_string()),
        None => basename.to_owned(),
    };
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join(basename))?)
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
    let barrier = std::env::var_os("ENGRAM_FIXTURE_BARRIER").map(PathBuf::from);
    if barrier
        .as_ref()
        .is_some_and(|path| !direct_child(&root, path))
    {
        return Err("barrier must be a direct child of the state directory".into());
    }
    let completion_barrier = evidence::barrier(
        &root,
        "ENGRAM_FIXTURE_COMPLETION_BARRIER",
        "completion-release",
    )?;
    let premature_barrier = evidence::barrier(
        &root,
        "ENGRAM_FIXTURE_PREMATURE_BARRIER",
        "premature-release",
    )?;
    let plan = plan::Plan::from_env()?;
    let tool = if plan.is_some() {
        "plan_pending".into()
    } else {
        std::env::var("ENGRAM_FIXTURE_TOOL")?
    };
    if !plan::tool_name(&tool) {
        return Err("invalid fixture tool".into());
    }
    let mode = std::env::var("ENGRAM_FIXTURE_MODE")?;
    if !["read", "write", "denied", "refire", "held-call", "forged"].contains(&mode.as_str()) {
        return Err("unsupported fixture mode".into());
    }
    let arguments = configured_object("ENGRAM_FIXTURE_ARGUMENTS_JSON", 4096, 16)?;
    let expected = configured_object("ENGRAM_FIXTURE_EXPECTED_RESULT_JSON", 1024 * 1024, 128)?;
    let configured_id = match std::env::var("ENGRAM_FIXTURE_CALL_ID") {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(_) => return Err("invalid fixture call ID".into()),
    };
    let extended_proof =
        plan.is_some() || arguments.is_some() || expected.is_some() || configured_id.is_some();
    let call_id = configured_id.unwrap_or_else(|| "fixture-call".into());
    if !plan::identifier(&call_id) {
        return Err("invalid fixture call ID".into());
    }
    let arguments = arguments.unwrap_or_else(|| json!({"fixture":mode}));
    let proof = proof_file(&root, &proof_path, std::process::id(), || {
        File::open("/dev/urandom")
    })?;
    Ok(Config {
        tool,
        mode,
        proof,
        barrier,
        completion_barrier,
        premature_barrier,
        absent: false,
        premature: false,
        arguments,
        call_id,
        expected,
        extended_proof,
        selected_schema: None,
        exposed_hash: None,
        exposed_tools: Vec::new(),
        plan,
        root,
        marker: None,
        native_json: false,
        harness: "unset",
    })
}
impl Config {
    fn bind_tools(&mut self, tools: &[Value], harness: &'static str) -> Result<()> {
        self.harness = harness;
        if self.extended_proof {
            self.exposed_hash = Some(plan::exposed_mcp_hash(tools)?);
        }
        self.exposed_tools = tools.to_vec();
        if self.plan.is_none() {
            self.validate_selected()?;
        }
        Ok(())
    }
    fn validate_selected(&mut self) -> Result<()> {
        let selected: Vec<_> = self
            .exposed_tools
            .iter()
            .filter(|tool| tool.get("name") == Some(&json!(self.tool)))
            .collect();
        if self.absent {
            if !selected.is_empty() {
                return Err("negative alias is exposed".into());
            }
            self.selected_schema = None;
            return Ok(());
        }
        if selected.len() != 1
            || !selected[0]["inputSchema"].is_object()
            || (self.harness == "codex" && !selected[0]["description"].is_string())
        {
            return Err("expected tool missing, duplicated, or malformed".into());
        }
        self.selected_schema = Some(digest(&selected[0]["inputSchema"])?);
        Ok(())
    }
    fn select(&mut self, prompt: Option<&Value>) -> Result<()> {
        let Some(plan) = &self.plan else {
            return Ok(());
        };
        let step = match prompt {
            Some(value) => plan.select(&plan::prompt_text(value, self.harness == "claude")?)?,
            None => plan.resume(&self.root, self.harness, &self.mode)?,
        };
        self.tool = step.tool;
        self.arguments = step.arguments;
        self.call_id = step.call_id;
        self.expected = step.expected;
        self.native_json = step.native_json;
        self.absent = step.absent;
        self.premature = step.premature;
        self.marker = Some(step.marker);
        self.validate_selected()?;
        Ok(())
    }
    fn persist_selection(&self, pending: bool) -> Result<()> {
        if let Some(plan) = &self.plan {
            plan.persist(
                &self.root,
                self.harness,
                &self.mode,
                self.marker.as_deref().ok_or("missing selected marker")?,
                pending,
            )?;
        }
        Ok(())
    }
}

fn record(cfg: &mut Config, mut proof: Value, native: Option<&Value>) -> Result<()> {
    if cfg.extended_proof {
        proof["selectedToolName"] = json!(cfg.tool);
        if !cfg.absent {
            proof["selectedSchemaSha256"] = json!(cfg
                .selected_schema
                .as_ref()
                .ok_or("missing selected schema")?);
        }
        proof["exposedMcpToolNamesSha256"] = json!(cfg
            .exposed_hash
            .as_ref()
            .ok_or("missing exposed MCP hash")?);
    }
    if let Some(plan) = &cfg.plan {
        proof["planSha256"] = json!(plan.hash);
    }
    if let Some(native) = native.filter(|_| cfg.expected.is_some() || cfg.native_json) {
        if !native.is_object() || serde_json::to_vec(native)?.len() > 1024 * 1024 {
            return Err("invalid native result size or shape".into());
        }
        let actual_hash = digest(native)?;
        if let Some(expected) = &cfg.expected {
            let expected_hash = digest(expected)?;
            proof["resultMatchesExpected"] = json!(actual_hash == expected_hash);
            proof["expectedResultSha256"] = json!(expected_hash);
        }
        proof["resultSha256"] = json!(actual_hash);
    }
    writeln!(cfg.proof, "{proof}")?;
    cfg.proof.flush()?;
    cfg.persist_selection(false)?;
    Ok(())
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
    let mut initialized = false;
    let mut thread_ready = false;
    let mut pending = false;
    let input = stdin_lines();
    while let Ok(line) = input.recv() {
        let value: Value = serde_json::from_str(&line?)?;
        if value.get("method").is_none() && value.get("id") == Some(&json!(77)) && pending {
            if value.get("error").is_some() {
                return Err("dynamic transport error".into());
            }
            let result = value
                .get("result")
                .and_then(Value::as_object)
                .ok_or("missing dynamic result")?;
            let success = result
                .get("success")
                .and_then(Value::as_bool)
                .ok_or("missing success")?;
            if cfg.absent {
                if success
                    || result.get("contentItems")
                        != Some(
                            &json!([{"type":"inputText","text":format!("undeclared dynamic tool: {}", cfg.tool)}]),
                        )
                {
                    return Err("absent alias was not rejected".into());
                }
                let proof = json!({"callId":cfg.call_id,"tool":cfg.tool,"expectExposure":"absent","requestAttempted":true,"protocolDenialCode":"unknown_tool"});
                record(&mut cfg, proof, None)?;
                cfg.native_completion(&value, &input)?;
                emit(
                    json!({"method":"turn/completed","params":{"threadId":"fixture-thread","turn":{"id":"fixture-turn","status":"completed"}}}),
                )?;
                pending = false;
                continue;
            }
            if cfg.mode == "forged" {
                if success
                    || result.get("contentItems")
                        != Some(
                            &json!([{"type":"inputText","text":"undeclared dynamic tool: fixture_forged"}]),
                        )
                {
                    return Err("forged tool was not rejected".into());
                }
                let proof =
                    json!({"callId":cfg.call_id,"tool":"fixture_forged","forgedRejected":true});
                record(&mut cfg, proof, None)?;
                emit(
                    json!({"method":"turn/completed","params":{"threadId":"fixture-thread","turn":{"id":"fixture-turn","status":"completed"}}}),
                )?;
                pending = false;
                continue;
            }
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
            if cfg.native_json {
                let proof = json!({"callId":cfg.call_id,"tool":cfg.tool,"resultReceived":true,"success":success});
                record(&mut cfg, proof, value.get("result"))?;
                cfg.native_completion(&value, &input)?;
                emit(
                    json!({"method":"turn/completed","params":{"threadId":"fixture-thread","turn":{"id":"fixture-turn","status":"completed"}}}),
                )?;
                pending = false;
                continue;
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
            let receipt: Value = serde_json::from_str(
                items
                    .last()
                    .and_then(|item| item.get("text"))
                    .and_then(Value::as_str)
                    .ok_or("missing receipt")?,
            )?;
            let receipt = &receipt;
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
            let proof = json!({"callId":cfg.call_id,"tool":cfg.tool,"resultReceived":true,"success":success,"textSentinel":text_sentinel,"textSequence":text_sequence,"structuredSentinel":structured,"isError":is_error,"outcome":outcome,"delivery":delivery,"resultUnavailable":unavailable,"denialPlanMode":denial_plan});
            let native = value.get("result").filter(|_| delivery == "available");
            record(&mut cfg, proof, native)?;
            cfg.native_completion(&value, &input)?;
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
                cfg.bind_tools(tools, "codex")?;
                thread_ready = true;
                emit(
                    json!({"id":id.ok_or("missing id")?,"result":{"thread":{"id":"fixture-thread","turns":[]}}}),
                )?;
            }
            "turn/start" if thread_ready && !pending => {
                cfg.select(Some(&value))?;
                cfg.prepare_evidence()?;
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
                cfg.persist_selection(true)?;
                emit(
                    json!({"id":77,"method":"item/tool/call","params":{"callId":cfg.call_id,"tool":if cfg.mode == "forged" { "fixture_forged" } else { &cfg.tool },"arguments":cfg.arguments,"threadId":"fixture-thread","turnId":"fixture-turn"}}),
                )?;
                if cfg.premature {
                    cfg.premature_completion(&input)?;
                    emit(
                        json!({"method":"turn/completed","params":{"threadId":"fixture-thread","turn":{"id":"fixture-turn","status":"completed"}}}),
                    )?;
                } else {
                    pending = true;
                }
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
    let initialized = receive(&bridge_rx)?;
    let result = initialized.get("result");
    if initialized["jsonrpc"] != "2.0"
        || initialized["id"] != 1
        || initialized.get("error").is_some()
        || result
            .and_then(|v| v.get("protocolVersion"))
            .and_then(Value::as_str)
            != Some("2025-06-18")
        || !result
            .and_then(|v| v.get("capabilities"))
            .is_some_and(Value::is_object)
        || !["name", "version"].iter().all(|field| {
            result
                .and_then(|v| v.get("serverInfo"))
                .and_then(|v| v.get(*field))
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
        })
    {
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
    cfg.bind_tools(tools, "claude")?;
    emit(json!({"type":"system","subtype":"init","session_id":"fixture-session"}))?;
    let input = stdin_lines();
    let mut call_request_id = 3_u64;
    // Deferred calls can re-fire without a prompt. Completed plan selections
    // wait for the next current prompt when the conversation resumes.
    let mut resumed = ["write", "refire"].contains(&cfg.mode.as_str())
        && args.iter().any(|arg| arg == "--resume")
        && match &cfg.plan {
            Some(plan) => plan.has_pending_selection(&cfg.root, "claude", &cfg.mode)?,
            None => true,
        };
    loop {
        let no_current_prompt = std::mem::take(&mut resumed);
        let prompt: Value = if no_current_prompt {
            json!({"type":"user"})
        } else {
            match input.recv() {
                Ok(line) => serde_json::from_str(&line?)?,
                Err(_) => break,
            }
        };
        if prompt["type"] != "user" {
            return Err("unsupported Claude input".into());
        }
        cfg.select(if no_current_prompt {
            None
        } else {
            Some(&prompt)
        })?;
        cfg.prepare_evidence()?;
        cfg.persist_selection(true)?;
        let mut hook = ChildGuard(
            Command::new(command)
                .arg("hook-bridge")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?,
        );
        let hook_rx = read_lines(&mut hook.0)?;
        let arguments = cfg.arguments.clone();
        let call_tool = if cfg.mode == "forged" {
            "fixture_forged"
        } else {
            &cfg.tool
        };
        emit(
            json!({"type":"assistant","message":{"id":"fixture-message","content":[{"type":"tool_use","id":cfg.call_id,"name":format!("mcp__engrams__{call_tool}"),"input":arguments}]}}),
        )?;
        child_send(
            &mut hook.0,
            json!({"tool_name":format!("mcp__engrams__{}",call_tool),"tool_use_id":cfg.call_id,"tool_input":arguments,"permission_mode":"default"}),
        )?;
        drop(hook.0.stdin.take());
        let Some(decision) = interruptible_receive(&hook_rx, &input)? else {
            continue;
        };
        match decision
            .pointer("/hookSpecificOutput/permissionDecision")
            .and_then(Value::as_str)
        {
            Some("defer") if cfg.premature => {
                cfg.premature_completion(&input)?;
                emit(
                    json!({"type":"result","subtype":"success","session_id":"fixture-session","result":"fixture"}),
                )?;
                continue;
            }
            _ if cfg.premature
                || cfg.absent
                    && decision.pointer("/hookSpecificOutput/permissionDecision")
                        != Some(&json!("allow")) =>
            {
                return Err("unexpected evidence hook decision".into());
            }
            Some("allow") => {}
            Some("defer") | Some("deny") => {
                emit(
                    json!({"type":"result","subtype":"success","session_id":"fixture-session","result":"fixture","terminal_reason":"tool_deferred"}),
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
            json!({"jsonrpc":"2.0","id":call_request_id,"method":"tools/call","params":{"name":call_tool,"arguments":arguments,"_meta":{"claudecode/toolUseId":cfg.call_id}}}),
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
            let proof = json!({"callId":cfg.call_id,"tool":"fixture_forged","forgedRejected":true});
            record(&mut cfg, proof, None)?;
            emit(
                json!({"type":"result","subtype":"success","session_id":"fixture-session","result":"fixture"}),
            )?;
            break;
        }
        if response.get("result").is_some() && response.get("error").is_some() {
            return Err("ambiguous tool response".into());
        }
        let result = response.get("result");
        if cfg.absent {
            if result.is_some()
                || response.pointer("/error/code") != Some(&json!(-32602))
                || response.pointer("/error/message") != Some(&json!("Unknown tool"))
            {
                return Err("absent alias was not rejected".into());
            }
            let proof = json!({"callId":cfg.call_id,"tool":cfg.tool,"expectExposure":"absent","requestAttempted":true,"protocolDenialCode":"unknown_tool","nativeDenialCode":-32602});
            record(&mut cfg, proof, None)?;
            cfg.native_completion(&response, &input)?;
            emit(
                json!({"type":"result","subtype":"success","session_id":"fixture-session","result":"fixture"}),
            )?;
            continue;
        }
        if cfg.native_json {
            let native = result.ok_or("missing native JSON result")?;
            let content = native
                .get("content")
                .and_then(Value::as_array)
                .ok_or("missing native JSON content")?;
            if content.iter().any(|item| {
                item.get("type") != Some(&json!("text"))
                    || !item.get("text").is_some_and(Value::is_string)
            }) {
                return Err("invalid native JSON content".into());
            }
            let proof = json!({"callId":cfg.call_id,"tool":cfg.tool,"resultReceived":true});
            record(&mut cfg, proof, Some(native))?;
            cfg.native_completion(&response, &input)?;
            emit(
                json!({"type":"result","subtype":"success","session_id":"fixture-session","result":"fixture"}),
            )?;
            continue;
        }
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
        let proof = json!({"callId":cfg.call_id,"tool":cfg.tool,"resultReceived":true,"textSentinel":text,"textSequence":text_sequence,"structuredSentinel":structured,"isError":is_error,"outcome":outcome,"delivery":delivery,"resultUnavailable":unavailable,"denialPlanMode":denial_plan});
        record(&mut cfg, proof, result)?;
        cfg.native_completion(&response, &input)?;
        emit(
            json!({"type":"result","subtype":"success","session_id":"fixture-session","result":"fixture"}),
        )?;
        // A completed deferred re-fire must leave this model process idle.
        // Exiting here would simulate a new crash and replay the completed
        // fixture call on every subsequent --resume startup.
        if cfg.plan.is_none() && !["write", "refire"].contains(&cfg.mode.as_str()) {
            break;
        }
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

#[cfg(test)]
mod proof_tests {
    use super::*;
    use std::io::Read;

    struct State(PathBuf);
    impl State {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("proof-unit-{}-{name}", std::process::id()));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }
    impl Drop for State {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn same_pid_launches_claim_distinct_files_and_collision_preserves_prior_bytes() {
        let state = State::new("same-pid");
        let template = state.0.join("proof-{launch}.json");
        let first = state
            .0
            .join("proof-42-000102030405060708090a0b0c0d0e0f.json");
        let second = state
            .0
            .join("proof-42-ffffffffffffffffffffffffffffffff.json");
        let nonce = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
        let mut initial =
            proof_file(&state.0, &template, 42, || Ok(io::Cursor::new(nonce))).unwrap();
        initial.write_all(b"retained prior observation\n").unwrap();
        initial.flush().unwrap();
        drop(initial);
        let mut fresh =
            proof_file(&state.0, &template, 42, || Ok(io::Cursor::new([255; 16]))).unwrap();
        fresh.write_all(b"fresh observation\n").unwrap();
        fresh.flush().unwrap();
        drop(fresh);
        let error = proof_file(&state.0, &template, 42, || Ok(io::Cursor::new(nonce))).unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&first).unwrap(), b"retained prior observation\n");
        assert_eq!(fs::read(&second).unwrap(), b"fresh observation\n");
        assert_eq!(fs::read_dir(&state.0).unwrap().count(), 2);
    }

    #[test]
    fn launch_entropy_failure_and_short_reads_claim_no_file() {
        let state = State::new("entropy");
        let template = state.0.join("proof-{launch}.json");
        for bytes in 0..16 {
            let error = proof_file(&state.0, &template, 42, || {
                Ok(io::Cursor::new(vec![0; bytes]))
            })
            .unwrap_err();
            assert_eq!(
                error.downcast_ref::<io::Error>().unwrap().kind(),
                io::ErrorKind::UnexpectedEof
            );
            assert_eq!(fs::read_dir(&state.0).unwrap().count(), 0);
        }
        let error = proof_file::<File>(&state.0, &template, 42, || {
            Err(io::Error::other("entropy unavailable"))
        })
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::Other
        );
        assert_eq!(fs::read_dir(&state.0).unwrap().count(), 0);
        struct FailedRead;
        impl Read for FailedRead {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("entropy read failed"))
            }
        }
        let error = proof_file(&state.0, &template, 42, || {
            Ok(io::repeat(0).take(8).chain(FailedRead))
        })
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<io::Error>().unwrap().kind(),
            io::ErrorKind::Other
        );
        assert_eq!(fs::read_dir(&state.0).unwrap().count(), 0);
    }

    #[test]
    fn literal_and_pid_paths_need_no_entropy_and_never_replace_existing_files() {
        let state = State::new("legacy");
        for (template, name) in [
            ("proof.json", "proof.json"),
            ("proof-{pid}.json", "proof-42.json"),
        ] {
            let path = state.0.join(template);
            let mut file =
                proof_file::<File>(&state.0, &path, 42, || panic!("unexpected entropy read"))
                    .unwrap();
            file.write_all(b"preserve\n").unwrap();
            file.flush().unwrap();
            drop(file);
            let error =
                proof_file::<File>(&state.0, &path, 42, || panic!("unexpected entropy read"))
                    .unwrap_err();
            assert_eq!(
                error.downcast_ref::<io::Error>().unwrap().kind(),
                io::ErrorKind::AlreadyExists
            );
            assert_eq!(fs::read(state.0.join(name)).unwrap(), b"preserve\n");
        }
    }
}
