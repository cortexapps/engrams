//! Linux production wrapper processes; only the model CLI is a fixture.
//! This is transport and presentation evidence, not VM isolation or server policy.
#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use engram_core::{SandboxId, SessionId};
use engram_harness_proto::{
    read_msg, write_msg, HarnessAttach, HarnessAttachAck, HarnessCommand, HarnessEvent,
    HarnessFrame,
};
use nix::sys::signal::{kill, killpg, Signal};
use nix::unistd::Pid;
use serde_json::{json, Value};
use tokio::net::{tcp::OwnedWriteHalf, TcpListener};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

const TOOL: &str = "remote_inventory";
const CALL: &str = "fixture-call";
const LIMIT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wrapper {
    Claude,
    Codex,
}
impl Wrapper {
    fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
    fn binary(self) -> PathBuf {
        artifact(match self {
            Self::Claude => "ENGRAM_TEST_CLAUDE_BIN",
            Self::Codex => "ENGRAM_TEST_CODEX_BIN",
        })
    }
}

fn artifact(variable: &str) -> PathBuf {
    let path = PathBuf::from(std::env::var_os(variable).expect("required current Linux artifact"));
    assert!(path.is_absolute());
    let path = path.canonicalize().unwrap();
    assert!(fs::metadata(&path).unwrap().permissions().mode() & 0o111 != 0);
    let bytes = fs::read(&path).unwrap();
    assert_eq!(
        &bytes[..4],
        b"\x7fELF",
        "a Linux production artifact is required"
    );
    assert_eq!(bytes[4], 2, "64-bit ELF required");
    let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
    assert_eq!(
        machine,
        if cfg!(target_arch = "aarch64") {
            183
        } else {
            62
        }
    );
    path
}

struct Process {
    child: Child,
    root: PathBuf,
}
impl Drop for Process {
    fn drop(&mut self) {
        // The wrapper and all fixture hook/bridge descendants share this owned group.
        let _ = killpg(
            Pid::from_raw(self.child.id().try_into().unwrap()),
            Signal::SIGKILL,
        );
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Harness {
    process: Process,
    writer: OwnedWriteHalf,
    events: mpsc::Receiver<HarnessEvent>,
    reader: tokio::task::JoinHandle<()>,
    trace: Vec<HarnessEvent>,
    fixture: PathBuf,
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.reader.abort();
    }
}
impl Harness {
    async fn start(wrapper: Wrapper, mode: &str, deferred: bool) -> Self {
        Self::start_with_plan(wrapper, mode, deferred, None).await
    }

    async fn start_with_plan(
        wrapper: Wrapper,
        mode: &str,
        deferred: bool,
        plan: Option<&Value>,
    ) -> Self {
        let base =
            PathBuf::from(std::env::var_os("ENGRAM_TEST_STATE_ROOT").expect("owned state root"));
        assert!(base.is_absolute());
        assert_eq!(
            base.canonicalize().unwrap(),
            base,
            "state parent must be canonical"
        );
        let root = base.join(format!("{}-{}", wrapper.name(), uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let proof = root.join("proof");
        fs::create_dir(&proof).unwrap();
        fs::set_permissions(&proof, fs::Permissions::from_mode(0o700)).unwrap();
        let fixture = artifact("ENGRAM_FIXTURE_BIN");
        let version = timeout(
            LIMIT,
            tokio::process::Command::new(&fixture)
                .arg("--fixture-version")
                .env_clear()
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("fixture version deadline")
        .unwrap();
        assert!(version.status.success());
        assert_eq!(version.stdout, b"engram-mcp-model-fixture 1\n");
        assert!(version.stderr.is_empty());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let session = SessionId::new();
        let sandbox = SandboxId::new();
        let manifest = if plan.is_some() {
            json!([])
        } else {
            json!([{"name":TOOL,"description":"Fixture inventory","inputSchema":{"type":"object","properties":{"fixture":{"type":"string"}},"required":["fixture"],"additionalProperties":false},"execution":if deferred {"deferred"} else {"sync"},"nativeBindings":{},"resultFormat":"mcp"}])
        };
        let mut command = Command::new(wrapper.binary());
        command
            .env_clear()
            .current_dir(&root)
            .process_group(0)
            .args([
                "--connect",
                &listener.local_addr().unwrap().to_string(),
                "--session-id",
                &session.to_string(),
            ])
            .arg("--state-dir")
            .arg(root.join("state"))
            .arg(format!("--{}-bin", wrapper.name()))
            .arg(&fixture)
            .env("HOME", &root)
            .env("TMPDIR", &root)
            .env("RUST_LOG", "off")
            .env("ENGRAM_ATTACH_TOKEN_FILE", root.join("absent-token"))
            .env("ENGRAM_SANDBOX_ID", sandbox.to_string())
            .env("ENGRAM_BINDING_EPOCH", "1")
            .env("ENGRAM_TOOLS", manifest.to_string())
            .env("ENGRAM_FIXTURE_MODE", mode)
            .env("ENGRAM_FIXTURE_STATE_DIR", &proof)
            .env("ENGRAM_FIXTURE_PROOF", proof.join("launch-{pid}.json"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(plan) = plan {
            command.env("ENGRAM_FIXTURE_CALL_PLAN_JSON", plan.to_string());
        } else {
            command.env("ENGRAM_FIXTURE_TOOL", TOOL);
        }
        if wrapper == Wrapper::Codex {
            command.env("CODEX_API_KEY", "fixture-placeholder");
        }
        let process = Process {
            child: command.spawn().unwrap(),
            root,
        };
        let (mut socket, _) = timeout(LIMIT, listener.accept())
            .await
            .expect("wrapper attach deadline")
            .unwrap();
        let attach: HarnessAttach = timeout(LIMIT, read_msg(&mut socket))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(attach.session_id, session);
        assert_eq!(attach.sandbox_id, sandbox);
        assert_eq!(attach.binding_epoch, 1);
        assert_eq!(
            attach.harness_version,
            format!(
                "engram-harness-{}/{}",
                wrapper.name(),
                env!("CARGO_PKG_VERSION")
            )
        );
        // HarnessAttach has no global wire-version negotiation. Current artifacts
        // and decoding actual frames establish this test's protocol provenance.
        write_msg(
            &mut socket,
            &HarnessAttachAck {
                ok: true,
                reject: None,
                message: None,
            },
        )
        .await
        .unwrap();
        let (mut read, writer) = socket.into_split();
        let (send, events) = mpsc::channel(128);
        let reader = tokio::spawn(async move {
            let mut previous = 0;
            let mut instance = None;
            while let Ok(frame) = read_msg::<_, HarnessFrame>(&mut read).await {
                let HarnessFrame::SeqEvent {
                    seq,
                    event,
                    binding_epoch,
                    incarnation,
                } = frame
                else {
                    panic!("expected sequenced event")
                };
                assert_eq!(binding_epoch, 1);
                assert_eq!(seq, previous + 1);
                previous = seq;
                if let Some(old) = instance {
                    assert_eq!(incarnation, old);
                }
                instance = Some(incarnation);
                if send.send(event).await.is_err() {
                    return;
                }
            }
        });
        let mut h = Self {
            process,
            writer,
            events,
            reader,
            trace: vec![],
            fixture,
        };
        h.until(|e| matches!(e, HarnessEvent::Idle)).await;
        h
    }
    async fn send(&mut self, command: HarnessCommand) {
        timeout(
            LIMIT,
            write_msg(&mut self.writer, &HarnessFrame::Command(command)),
        )
        .await
        .unwrap()
        .unwrap();
    }
    async fn event(&mut self) -> HarnessEvent {
        let event = timeout(LIMIT, self.events.recv())
            .await
            .expect("harness event deadline")
            .expect("wrapper event channel closed");
        self.trace.push(event.clone());
        write_msg(
            &mut self.writer,
            &HarnessFrame::EventAck {
                seq: self.trace.len() as u64,
            },
        )
        .await
        .unwrap();
        event
    }
    async fn until(&mut self, predicate: impl Fn(&HarnessEvent) -> bool) -> HarnessEvent {
        timeout(LIMIT, async {
            loop {
                let event = self.event().await;
                if predicate(&event) {
                    return event;
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "expected event deadline; observed kinds: {:?}; launches: {}",
                self.trace
                    .iter()
                    .map(HarnessEvent::kind)
                    .collect::<Vec<_>>(),
                self.proofs().len()
            )
        })
    }
    async fn prompt(&mut self) {
        self.send(HarnessCommand::Prompt {
            text: "fixture".into(),
            prompt_id: "fixture-prompt".into(),
            mode: None,
        })
        .await;
    }
    async fn requested(&mut self, mode: &str) {
        let event = self
            .until(|e| matches!(e, HarnessEvent::ToolCallRequested { .. }))
            .await;
        let HarnessEvent::ToolCallRequested {
            call_id,
            name,
            args_json,
            run_id,
        } = event
        else {
            unreachable!()
        };
        assert_eq!(call_id, CALL);
        assert_eq!(name, TOOL);
        assert!(!run_id.is_empty());
        assert_eq!(
            serde_json::from_str::<Value>(&args_json).unwrap(),
            json!({"fixture":mode})
        );
    }
    async fn result(&mut self, value: Value) {
        self.send(HarnessCommand::ToolResult {
            call_id: CALL.into(),
            result_json: value.to_string(),
        })
        .await;
    }
    fn proofs(&self) -> Vec<PathBuf> {
        let entries: Vec<_> = fs::read_dir(self.process.root.join("proof"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert!(entries.len() <= 4, "fixture launch budget exceeded");
        entries
    }
    async fn proof(&mut self) -> Value {
        timeout(LIMIT, async {
            loop {
                for path in self.proofs() {
                    if let Ok(value) = serde_json::from_slice::<Value>(&fs::read(path).unwrap()) { return value; }
                }
                tokio::select! {
                    event = self.events.recv() => {
                        let event = event.expect("wrapper closed before proof");
                        self.trace.push(event);
                        write_msg(&mut self.writer, &HarnessFrame::EventAck { seq:self.trace.len() as u64 }).await.unwrap();
                    }
                    _ = sleep(Duration::from_millis(10)) => {}
                }
            }
        }).await.expect("model CLI result proof deadline")
    }
    fn requests(&self) -> usize {
        self.trace
            .iter()
            .filter(|e| matches!(e, HarnessEvent::ToolCallRequested { .. }))
            .count()
    }
    async fn finish(mut self, expected_requests: usize) -> Vec<HarnessEvent> {
        self.send(HarnessCommand::Shutdown { grace_secs: 1 }).await;
        // The SDK drains its event outbox before exit. Continue ACKing terminal
        // events while waiting for the production wrapper to reap its CLI.
        timeout(LIMIT, async {
            let mut open = true;
            loop {
                if let Some(status) = self.process.child.try_wait().unwrap() {
                    assert!(status.success(), "wrapper shutdown failed");
                    break;
                }
                tokio::select! {
                    event = self.events.recv(), if open => {
                        if let Some(event) = event {
                            self.trace.push(event);
                            let _ = write_msg(&mut self.writer, &HarnessFrame::EventAck { seq: self.trace.len() as u64 }).await;
                        } else { open = false; }
                    }
                    _ = sleep(Duration::from_millis(10)) => {}
                }
            }
        }).await.expect("wrapper shutdown deadline");
        timeout(LIMIT, &mut self.reader)
            .await
            .expect("reader exit deadline")
            .expect("sequence reader failed");
        while let Ok(event) = self.events.try_recv() {
            self.trace.push(event);
        }
        assert_eq!(self.requests(), expected_requests);
        self.trace.clone()
    }
    async fn kill_first_cli(&mut self) {
        let paths = self.proofs();
        assert_eq!(paths.len(), 1, "one first fixture launch");
        let name = paths[0].file_stem().unwrap().to_str().unwrap();
        let pid: i32 = name.strip_prefix("launch-").unwrap().parse().unwrap();
        let proc = PathBuf::from(format!("/proc/{pid}"));
        assert_eq!(fs::read_link(proc.join("exe")).unwrap(), self.fixture);
        let stat = fs::read_to_string(proc.join("stat")).unwrap();
        let fields: Vec<_> = stat
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        assert_eq!(
            fields[1].parse::<u32>().unwrap(),
            self.process.child.id(),
            "owned direct model child"
        );
        assert_eq!(
            fields[2].parse::<u32>().unwrap(),
            self.process.child.id(),
            "owned process group"
        );
        kill(Pid::from_raw(pid), Signal::SIGKILL).unwrap();
        timeout(LIMIT, async {
            while proc.exists() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("first CLI reaped");
        timeout(LIMIT, async {
            while self.proofs().len() < 2 {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("replacement CLI launch");
        assert!(
            fs::read(&paths[0]).unwrap().is_empty(),
            "dead CLI received no result"
        );
    }
}

fn output(error: bool) -> Value {
    json!({"outcome":if error {"failed"} else {"succeeded"},"delivery":"available","output":{"content":[{"type":"text","text":"sentinel"},{"type":"text","text":"second sentinel"}],"structuredContent":{"fixture":true},"isError":error},"resultUnavailable":false})
}
fn assert_receipt(proof: &Value, error: bool) {
    assert_eq!(proof["callId"], CALL);
    assert_eq!(proof["tool"], TOOL);
    for field in [
        "resultReceived",
        "textSentinel",
        "textSequence",
        "structuredSentinel",
    ] {
        assert_eq!(proof[field], true, "{field}");
    }
    assert_eq!(proof["isError"], error);
    assert_eq!(proof["outcome"], if error { "failed" } else { "succeeded" });
    assert_eq!(proof["delivery"], "available");
    assert_eq!(proof["resultUnavailable"], false);
    if let Some(success) = proof.get("success") {
        assert_eq!(success, &json!(!error));
    }
}

async fn delivered(mode: &str, value: Value, check: fn(&Value), deferred: bool) {
    for wrapper in [Wrapper::Claude, Wrapper::Codex] {
        timeout(Duration::from_secs(45), async {
            let mut h = Harness::start(wrapper, mode, deferred).await;
            h.prompt().await;
            h.requested(mode).await;
            if deferred {
                h.until(|e| matches!(e, HarnessEvent::Parked)).await;
            }
            h.result(value.clone()).await;
            let proof = h.proof().await;
            check(&proof);
            assert_eq!(h.requests(), 1);
            h.finish(1).await;
        })
        .await
        .expect("scenario deadline");
    }
}

#[tokio::test]
#[ignore = "requires current Linux wrapper and fixture artifacts"]
async fn read_receipt_and_ordered_content() {
    delivered("read", output(false), |v| assert_receipt(v, false), false).await;
}
#[tokio::test]
#[ignore = "requires current Linux wrapper and fixture artifacts"]
async fn upstream_error_preserves_output() {
    delivered("read", output(true), |v| assert_receipt(v, true), false).await;
}
#[tokio::test]
#[ignore = "requires current Linux wrapper and fixture artifacts"]
async fn success_with_unavailable_delivery_is_distinct() {
    delivered("read", json!({"outcome":"succeeded","delivery":"unavailable","reason":"unsupported_content","resultUnavailable":true}), |v| {
        assert_eq!(v["outcome"], "succeeded"); assert_eq!(v["delivery"], "unavailable"); assert_eq!(v["resultUnavailable"], true);
        assert_eq!(v["isError"], Value::Null); assert_eq!(v["textSentinel"], false); assert_eq!(v["structuredSentinel"], false);
        if let Some(success) = v.get("success") { assert_eq!(success, &json!(false)); }
    }, false).await;
}
#[tokio::test]
#[ignore = "requires current Linux wrapper and fixture artifacts"]
async fn denied_runtime_receipt_does_not_invent_an_outcome() {
    delivered("denied", json!({"source":"runtime","delivery":"available","denial":"plan_mode","output":{"content":[{"type":"text","text":"denied"}],"isError":true},"resultUnavailable":false}), |v| {
        assert_eq!(v["denialPlanMode"], true); assert_eq!(v["isError"], true); assert_eq!(v["outcome"], Value::Null); assert_eq!(v["delivery"], "available");
    }, false).await;
}
#[tokio::test]
#[ignore = "requires current Linux wrapper and fixture artifacts"]
async fn deferred_write_delivers_after_park() {
    delivered("write", output(false), |v| assert_receipt(v, false), true).await;
}
#[tokio::test]
#[ignore = "requires current Linux wrapper and fixture artifacts"]
async fn omitted_forged_tool_is_rejected_by_wrapper() {
    for wrapper in [Wrapper::Claude, Wrapper::Codex] {
        let mut h = Harness::start(wrapper, "forged", false).await;
        h.prompt().await;
        assert_eq!(
            h.proof().await,
            json!({"callId":CALL,"tool":"fixture_forged","forgedRejected":true})
        );
        let trace = h.finish(0).await;
        assert!(trace
            .iter()
            .any(|e| matches!(e, HarnessEvent::RunCompleted { .. })));
    }
}

#[tokio::test]
#[ignore = "requires current Linux wrapper and fixture artifacts"]
async fn empty_manifest_absent_tool_returns_native_denial_proof() {
    let plan = json!({"steps":[{"marker":"missing","tool":"mcp_read_counter","arguments":{},"callId":"grant-missing","resultFormat":"mcp","expectExposure":"absent"}]});
    let mut h = Harness::start_with_plan(Wrapper::Claude, "read", false, Some(&plan)).await;
    h.send(HarnessCommand::Prompt {
        text: "[[fixture:missing]]".into(),
        prompt_id: "fixture-missing".into(),
        mode: None,
    })
    .await;
    h.until(|event| matches!(event, HarnessEvent::RunCompleted { .. }))
        .await;
    let proofs: Vec<_> = h
        .proofs()
        .into_iter()
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("launch-")
        })
        .collect();
    assert_eq!(proofs.len(), 1);
    let proof: Value = serde_json::from_slice(&fs::read(&proofs[0]).unwrap()).unwrap();
    assert_eq!(
        proof,
        json!({
            "callId":"grant-missing",
            "tool":"mcp_read_counter",
            "selectedToolName":"mcp_read_counter",
            "planSha256":"sha256:8e9af5f9f44fe85c47bd540d29ca82f2084934aef94a5fb9ce9655439e06a3c5",
            "exposedMcpToolNamesSha256":"sha256:4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945",
            "expectExposure":"absent",
            "requestAttempted":true,
            "protocolDenialCode":"unknown_tool",
            "nativeDenialCode":-32602
        })
    );
    h.finish(0).await;
}
#[tokio::test]
#[ignore = "requires current Linux wrapper and fixture artifacts"]
async fn held_call_interrupt_emits_cancel_without_result_claim() {
    for wrapper in [Wrapper::Claude, Wrapper::Codex] {
        let mut h = Harness::start(wrapper, "held-call", false).await;
        h.prompt().await;
        h.requested("held-call").await;
        h.send(HarnessCommand::Interrupt).await;
        let mut canceled = false;
        let mut interrupted = false;
        timeout(LIMIT, async {
            while !canceled || !interrupted {
                match h.event().await {
                    HarnessEvent::ToolCallCancelRequested { call_id, name, .. } => {
                        assert_eq!(call_id, CALL);
                        assert_eq!(name, TOOL);
                        canceled = true;
                    }
                    HarnessEvent::RunInterrupted { .. } => interrupted = true,
                    HarnessEvent::ToolCallCompleted { .. } => panic!("held call has no result"),
                    _ => {}
                }
            }
        })
        .await
        .expect("cancel and interrupt deadline");
        assert!(h.proofs().iter().all(|p| fs::read(p).unwrap().is_empty()));
        assert_eq!(h.requests(), 1);
        // No late result is sent: this proves the cancellation signal only,
        // never an external effect, rollback, or late-delivery guarantee.
        h.finish(1).await;
    }
}
#[tokio::test]
#[ignore = "requires current Linux wrapper and fixture artifacts"]
async fn stable_call_id_refires_after_model_cli_death() {
    for wrapper in [Wrapper::Claude, Wrapper::Codex] {
        timeout(Duration::from_secs(45), async {
            let mut h = Harness::start(wrapper, "refire", true).await;
            h.prompt().await;
            h.requested("refire").await;
            h.until(|e| matches!(e, HarnessEvent::Parked)).await;
            h.kill_first_cli().await;
            if wrapper == Wrapper::Codex {
                // Codex resumes its thread, then a new model turn reissues the
                // same native call ID. It does not promise Claude-style auto-refire.
                h.until(|e| matches!(e, HarnessEvent::Idle)).await;
                h.send(HarnessCommand::Prompt {
                    text: "fixture refire".into(),
                    prompt_id: "fixture-refire".into(),
                    mode: None,
                })
                .await;
                h.requested("refire").await;
            }
            h.result(output(false)).await;
            assert_receipt(&h.proof().await, false);
            assert_eq!(h.requests(), if wrapper == Wrapper::Claude { 1 } else { 2 });
            h.finish(if wrapper == Wrapper::Claude { 1 } else { 2 })
                .await;
        })
        .await
        .expect("refire scenario deadline");
    }
}

/// Only the external supervisor invokes this case. It SIGKILLs this process
/// after both real wrappers are holding calls; ordinary suite selection skips it.
#[tokio::test]
#[ignore = "requires the external deadline supervisor"]
async fn supervisor_timeout_fixture() {
    let mut held = Vec::new();
    let mut pairs = Vec::new();
    for wrapper in [Wrapper::Claude, Wrapper::Codex] {
        let mut h = Harness::start(wrapper, "held-call", false).await;
        h.prompt().await;
        h.requested("held-call").await;
        let proofs = h.proofs();
        assert_eq!(proofs.len(), 1);
        let fixture: u32 = proofs[0]
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .strip_prefix("launch-")
            .unwrap()
            .parse()
            .unwrap();
        pairs.push([h.process.child.id(), fixture]);
        held.push(h);
    }
    let root = PathBuf::from(std::env::var_os("ENGRAM_TEST_STATE_ROOT").unwrap());
    let staged = root.join("cleanup-ready.tmp");
    fs::write(&staged, serde_json::to_vec(&pairs).unwrap()).unwrap();
    fs::rename(staged, root.join("cleanup-ready")).unwrap();
    std::future::pending::<()>().await;
    drop(held);
}
