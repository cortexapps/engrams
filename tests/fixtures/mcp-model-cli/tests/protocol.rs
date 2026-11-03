use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};

struct Fixture(Child);
struct State(std::path::PathBuf);

impl State {
    fn new(name: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::var_os("TMPDIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")))
            .join(format!("fixture-state-{}-{name}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}

impl Drop for State {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn send(child: &mut Child, value: Value) {
    let input = child.stdin.as_mut().unwrap();
    writeln!(input, "{value}").unwrap();
    input.flush().unwrap();
}

fn wait_for_failure(child: &mut Child) {
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(!status.success());
            return;
        }
        assert!(std::time::Instant::now() < deadline, "fixture did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn claude_rejects_failed_or_malformed_initialize_even_when_list_works() {
    for case in [
        "error",
        "missing",
        "malformed",
        "server-info",
        "version",
        "both",
    ] {
        let state = State::new(case);
        let mock = env!("CARGO_BIN_EXE_fixture-mock-bridge");
        let config = state.0.join("config.json");
        let settings = state.0.join("settings.json");
        let proof = state.0.join("proof.json");
        std::fs::write(state.0.join("bad-initialize"), case).unwrap();
        std::fs::write(
            &config,
            json!({"mcpServers":{"engrams":{"type":"stdio","command":mock,"args":["mcp-bridge"]}}})
                .to_string(),
        )
        .unwrap();
        std::fs::write(
            &settings,
            json!({"hooks":{"PreToolUse":[{"hooks":[{"command":format!("{mock} hook-bridge")}]}]}})
                .to_string(),
        )
        .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_engram-mcp-model-fixture"))
            .args(["--print", "--strict-mcp-config", "--mcp-config"])
            .arg(config)
            .arg("--settings")
            .arg(settings)
            .env("ENGRAM_FIXTURE_TOOL", "remote_inventory")
            .env("ENGRAM_FIXTURE_MODE", "read")
            .env("ENGRAM_FIXTURE_STATE_DIR", &state.0)
            .env("ENGRAM_FIXTURE_PROOF", &proof)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut child = Fixture(child);
        send(
            &mut child.0,
            json!({"type":"user","message":{"role":"user","content":"fixture"}}),
        );
        drop(child.0.stdin.take());
        wait_for_failure(&mut child.0);
        assert!(std::fs::read(&proof).unwrap().is_empty());
    }
}

#[test]
fn codex_declares_and_delivers_the_requested_tool() {
    let state = State::new("delivery");
    let proof = state.0.join("proof.json");
    let child = Command::new(env!("CARGO_BIN_EXE_engram-mcp-model-fixture"))
        .arg("app-server")
        .env("ENGRAM_FIXTURE_TOOL", "remote_inventory")
        .env("ENGRAM_FIXTURE_MODE", "read")
        .env("ENGRAM_FIXTURE_STATE_DIR", &state.0)
        .env("ENGRAM_FIXTURE_PROOF", &proof)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = Fixture(child);
    let output = child.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let receive = || -> Value {
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap()).unwrap()
    };
    send(
        &mut child.0,
        json!({"id":1,"method":"initialize","params":{}}),
    );
    assert_eq!(receive()["id"], 1);
    send(
        &mut child.0,
        json!({"id":2,"method":"thread/start","params":{"dynamicTools":[{"name":"remote_inventory","description":"test","inputSchema":{"type":"object"}}]}}),
    );
    assert_eq!(receive()["result"]["thread"]["id"], "fixture-thread");
    send(
        &mut child.0,
        json!({"id":3,"method":"turn/start","params":{"threadId":"fixture-thread"}}),
    );
    assert_eq!(receive()["id"], 3);
    let call = receive();
    assert_eq!(call["method"], "item/tool/call");
    assert_eq!(call["params"]["tool"], "remote_inventory");
    assert_eq!(call["params"]["callId"], "fixture-call");
    send(
        &mut child.0,
        json!({"id":call["id"],"result":{"success":false,"contentItems":[{"type":"inputText","text":"sentinel"},{"type":"inputText","text":"second sentinel"},{"type":"inputText","text":"{\"structuredContent\":{\"fixture\":true}}"},{"type":"inputText","text":"{\"delivery\":\"available\",\"isError\":true,\"outcome\":\"failed\",\"resultUnavailable\":false}"}]}}),
    );
    assert_eq!(receive()["method"], "item/completed");
    assert_eq!(receive()["method"], "turn/completed");
    let recorded: Value = serde_json::from_slice(&std::fs::read(&proof).unwrap()).unwrap();
    assert_eq!(
        recorded,
        json!({"callId":"fixture-call","tool":"remote_inventory","resultReceived":true,"success":false,"textSentinel":true,"textSequence":true,"structuredSentinel":true,"isError":true,"outcome":"failed","delivery":"available","resultUnavailable":false,"denialPlanMode":false})
    );
}

#[test]
fn codex_rejects_a_thread_without_the_expected_tool() {
    let state = State::new("missing-tool");
    let child = Command::new(env!("CARGO_BIN_EXE_engram-mcp-model-fixture"))
        .arg("app-server")
        .env("ENGRAM_FIXTURE_TOOL", "remote_inventory")
        .env("ENGRAM_FIXTURE_MODE", "read")
        .env("ENGRAM_FIXTURE_STATE_DIR", &state.0)
        .env("ENGRAM_FIXTURE_PROOF", state.0.join("proof.json"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = Fixture(child);
    send(
        &mut child.0,
        json!({"id":0,"method":"initialize","params":{}}),
    );
    send(
        &mut child.0,
        json!({"id":1,"method":"thread/start","params":{"dynamicTools":[]}}),
    );
    drop(child.0.stdin.take());
    wait_for_failure(&mut child.0);
    let mut error = String::new();
    std::io::Read::read_to_string(&mut child.0.stderr.take().unwrap(), &mut error).unwrap();
    assert_eq!(error, "fixture protocol failure\n");
}

#[test]
fn rejects_proof_outside_the_owned_state_directory() {
    let state = State::new("root");
    let other = State::new("outside");
    let proof = other.0.join("proof.json");
    std::fs::write(&proof, b"preserve").unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_engram-mcp-model-fixture"))
        .arg("app-server")
        .env("ENGRAM_FIXTURE_TOOL", "remote_inventory")
        .env("ENGRAM_FIXTURE_MODE", "read")
        .env("ENGRAM_FIXTURE_STATE_DIR", &state.0)
        .env("ENGRAM_FIXTURE_PROOF", &proof)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = Fixture(child);
    wait_for_failure(&mut child.0);
    let mut error = String::new();
    std::io::Read::read_to_string(&mut child.0.stderr.take().unwrap(), &mut error).unwrap();
    assert_eq!(error, "fixture protocol failure\n");
    assert_eq!(std::fs::read(proof).unwrap(), b"preserve");
}

#[test]
fn claude_reads_generated_config_and_exchanges_mcp_and_hook_messages() {
    for mode in ["read", "write", "denied", "forged"] {
        let state = State::new(&format!("claude-{mode}"));
        let mock = env!("CARGO_BIN_EXE_fixture-mock-bridge");
        let config = state.0.join("config.json");
        let settings = state.0.join("settings.json");
        let proof = state.0.join("proof.json");
        std::fs::write(&config, json!({"mcpServers":{"engrams":{"type":"stdio","command":mock,"args":["mcp-bridge"],"alwaysLoad":true}}}).to_string()).unwrap();
        std::fs::write(&settings, json!({"hooks":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"command","command":format!("{mock} hook-bridge")}]}]}}).to_string()).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_engram-mcp-model-fixture"))
            .args(["--print", "--strict-mcp-config", "--mcp-config"])
            .arg(&config)
            .arg("--settings")
            .arg(&settings)
            .env("ENGRAM_FIXTURE_TOOL", "remote_inventory")
            .env("ENGRAM_FIXTURE_MODE", mode)
            .env("ENGRAM_FIXTURE_STATE_DIR", &state.0)
            .env("ENGRAM_FIXTURE_PROOF", &proof)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = Fixture(child);
        let output = child.0.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        send(
            &mut child.0,
            json!({"type":"user","message":{"role":"user","content":"fixture"}}),
        );
        let receive = || -> Value {
            serde_json::from_str(&rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap())
                .unwrap()
        };
        assert_eq!(receive()["subtype"], "init");
        assert_eq!(receive()["type"], "assistant");
        if mode != "forged" {
            assert_eq!(receive()["type"], "user");
        }
        assert_eq!(receive()["type"], "result");
        let recorded: Value = serde_json::from_slice(&std::fs::read(proof).unwrap()).unwrap();
        if mode == "forged" {
            assert_eq!(
                recorded,
                json!({"callId":"fixture-call","tool":"fixture_forged","forgedRejected":true})
            );
            continue;
        }
        let denied = mode == "denied";
        assert_eq!(
            recorded,
            json!({"callId":"fixture-call","tool":"remote_inventory","resultReceived":true,"textSentinel":true,"textSequence":true,"structuredSentinel":true,"isError":denied,"outcome":if denied { Value::Null } else { json!("succeeded") },"delivery":"available","resultUnavailable":false,"denialPlanMode":denied})
        );
    }
}

#[test]
fn codex_interrupts_a_closed_barrier_without_emitting_a_call() {
    let state = State::new("barrier");
    let child = Command::new(env!("CARGO_BIN_EXE_engram-mcp-model-fixture"))
        .arg("app-server")
        .env("ENGRAM_FIXTURE_TOOL", "remote_inventory")
        .env("ENGRAM_FIXTURE_MODE", "held-call")
        .env("ENGRAM_FIXTURE_STATE_DIR", &state.0)
        .env("ENGRAM_FIXTURE_PROOF", state.0.join("proof.json"))
        .env("ENGRAM_FIXTURE_BARRIER", state.0.join("release"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = Fixture(child);
    let output = child.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let receive = || -> Value {
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap()).unwrap()
    };
    send(
        &mut child.0,
        json!({"id":0,"method":"initialize","params":{}}),
    );
    assert_eq!(receive()["id"], 0);
    send(
        &mut child.0,
        json!({"id":1,"method":"thread/start","params":{"dynamicTools":[{"name":"remote_inventory","description":"test","inputSchema":{"type":"object"}}]}}),
    );
    assert_eq!(receive()["id"], 1);
    send(
        &mut child.0,
        json!({"id":2,"method":"turn/start","params":{}}),
    );
    assert_eq!(receive()["id"], 2);
    send(
        &mut child.0,
        json!({"id":3,"method":"turn/interrupt","params":{"threadId":"fixture-thread","turnId":"fixture-turn"}}),
    );
    assert_eq!(receive()["id"], 3);
    let completed = receive();
    assert_eq!(completed["method"], "turn/completed");
    assert_eq!(completed["params"]["turn"]["status"], "interrupted");
    assert!(std::fs::read(state.0.join("proof.json"))
        .unwrap()
        .is_empty());
}

#[test]
fn codex_interrupts_after_a_tool_call_is_sent() {
    let state = State::new("codex-held");
    let child = Command::new(env!("CARGO_BIN_EXE_engram-mcp-model-fixture"))
        .arg("app-server")
        .env("ENGRAM_FIXTURE_TOOL", "remote_inventory")
        .env("ENGRAM_FIXTURE_MODE", "held-call")
        .env("ENGRAM_FIXTURE_STATE_DIR", &state.0)
        .env("ENGRAM_FIXTURE_PROOF", state.0.join("proof.json"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = Fixture(child);
    let output = child.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let receive = || -> Value {
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap()).unwrap()
    };
    send(
        &mut child.0,
        json!({"id":0,"method":"initialize","params":{}}),
    );
    assert_eq!(receive()["id"], 0);
    send(
        &mut child.0,
        json!({"id":1,"method":"thread/start","params":{"dynamicTools":[{"name":"remote_inventory","description":"test","inputSchema":{"type":"object"}}]}}),
    );
    assert_eq!(receive()["id"], 1);
    send(
        &mut child.0,
        json!({"id":2,"method":"turn/start","params":{}}),
    );
    assert_eq!(receive()["id"], 2);
    assert_eq!(receive()["method"], "item/tool/call");
    send(
        &mut child.0,
        json!({"id":3,"method":"turn/interrupt","params":{"threadId":"fixture-thread","turnId":"fixture-turn"}}),
    );
    assert_eq!(receive()["id"], 3);
    assert_eq!(receive()["params"]["turn"]["status"], "interrupted");
    assert!(std::fs::read(state.0.join("proof.json"))
        .unwrap()
        .is_empty());
}

#[test]
fn claude_interrupts_an_inflight_bridge_call() {
    let state = State::new("claude-held");
    let mock = env!("CARGO_BIN_EXE_fixture-mock-bridge");
    let config = state.0.join("config.json");
    let settings = state.0.join("settings.json");
    std::fs::write(
        &config,
        json!({"mcpServers":{"engrams":{"type":"stdio","command":mock,"args":["mcp-bridge"]}}})
            .to_string(),
    )
    .unwrap();
    std::fs::write(
        &settings,
        json!({"hooks":{"PreToolUse":[{"hooks":[{"command":format!("{mock} hook-bridge")}]}]}})
            .to_string(),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_engram-mcp-model-fixture"))
        .args(["--print", "--strict-mcp-config", "--mcp-config"])
        .arg(config)
        .arg("--settings")
        .arg(settings)
        .env("ENGRAM_FIXTURE_TOOL", "remote_inventory")
        .env("ENGRAM_FIXTURE_MODE", "held-call")
        .env("ENGRAM_FIXTURE_STATE_DIR", &state.0)
        .env("ENGRAM_FIXTURE_PROOF", state.0.join("proof.json"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = Fixture(child);
    let output = child.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let receive = || -> Value {
        serde_json::from_str(&rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap()).unwrap()
    };
    send(
        &mut child.0,
        json!({"type":"user","message":{"role":"user","content":"fixture"}}),
    );
    assert_eq!(receive()["subtype"], "init");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !state.0.join("mock-pending").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "bridge call did not start"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    send(
        &mut child.0,
        json!({"type":"control_request","request_id":"interrupt-1","request":{"subtype":"interrupt"}}),
    );
    assert_eq!(receive()["type"], "assistant");
    assert_eq!(receive()["type"], "control_response");
    assert_eq!(receive()["subtype"], "error_during_execution");
    assert!(std::fs::read(state.0.join("proof.json"))
        .unwrap()
        .is_empty());
}

#[test]
fn claude_refires_the_same_call_after_process_death() {
    let state = State::new("refire");
    let mock = env!("CARGO_BIN_EXE_fixture-mock-bridge");
    let config = state.0.join("config.json");
    let settings = state.0.join("settings.json");
    std::fs::write(
        &config,
        json!({"mcpServers":{"engrams":{"type":"stdio","command":mock,"args":["mcp-bridge"]}}})
            .to_string(),
    )
    .unwrap();
    std::fs::write(
        &settings,
        json!({"hooks":{"PreToolUse":[{"hooks":[{"command":format!("{mock} hook-bridge")}]}]}})
            .to_string(),
    )
    .unwrap();
    for attempt in 0..2 {
        if attempt == 1 {
            std::fs::write(state.0.join("allow-refire"), b"ready").unwrap();
        }
        let proof = state.0.join(format!("proof-{attempt}.json"));
        let child = Command::new(env!("CARGO_BIN_EXE_engram-mcp-model-fixture"))
            .args(["--print", "--strict-mcp-config", "--mcp-config"])
            .arg(&config)
            .arg("--settings")
            .arg(&settings)
            .args(if attempt == 1 {
                vec!["--resume", "fixture-session"]
            } else {
                vec![]
            })
            .env("ENGRAM_FIXTURE_TOOL", "remote_inventory")
            .env("ENGRAM_FIXTURE_MODE", "refire")
            .env("ENGRAM_FIXTURE_STATE_DIR", &state.0)
            .env("ENGRAM_FIXTURE_PROOF", &proof)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = Fixture(child);
        let output = child.0.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let receive = || -> Value {
            serde_json::from_str(&rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap())
                .unwrap()
        };
        if attempt == 0 {
            send(
                &mut child.0,
                json!({"type":"user","message":{"role":"user","content":"fixture"}}),
            );
        }
        assert_eq!(receive()["subtype"], "init");
        assert_eq!(receive()["type"], "assistant");
        if attempt == 1 {
            assert_eq!(receive()["type"], "user");
        }
        assert_eq!(receive()["type"], "result");
        if attempt == 0 {
            assert!(std::fs::read(&proof).unwrap().is_empty());
        } else {
            let recorded: Value = serde_json::from_slice(&std::fs::read(&proof).unwrap()).unwrap();
            assert_eq!(recorded["callId"], "fixture-call");
            assert_eq!(recorded["resultReceived"], true);
        }
    }
}

#[test]
fn codex_write_denied_and_unavailable_calls_keep_the_native_result() {
    for (mode, case) in [
        ("write", "write"),
        ("denied", "denied"),
        ("read", "unavailable"),
    ] {
        let state = State::new(case);
        let proof = state.0.join("proof.json");
        let child = Command::new(env!("CARGO_BIN_EXE_engram-mcp-model-fixture"))
            .arg("app-server")
            .env("ENGRAM_FIXTURE_TOOL", "remote_inventory")
            .env("ENGRAM_FIXTURE_MODE", mode)
            .env("ENGRAM_FIXTURE_STATE_DIR", &state.0)
            .env("ENGRAM_FIXTURE_PROOF", &proof)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut child = Fixture(child);
        let output = child.0.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let receive = || -> Value {
            serde_json::from_str(&rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap())
                .unwrap()
        };
        send(
            &mut child.0,
            json!({"id":0,"method":"initialize","params":{}}),
        );
        assert_eq!(receive()["id"], 0);
        send(
            &mut child.0,
            json!({"id":1,"method":"thread/resume","params":{"threadId":"fixture-thread","dynamicTools":[{"name":"remote_inventory","description":"test","inputSchema":{"type":"object"}}]}}),
        );
        assert_eq!(receive()["id"], 1);
        send(
            &mut child.0,
            json!({"id":2,"method":"turn/start","params":{}}),
        );
        assert_eq!(receive()["id"], 2);
        let call = receive();
        assert_eq!(call["params"]["arguments"]["fixture"], mode);
        let success = case == "write";
        let receipt = if success {
            json!({"outcome":"succeeded","delivery":"available","isError":false,"resultUnavailable":false})
        } else if case == "unavailable" {
            json!({"outcome":"succeeded","delivery":"unavailable","reason":"result_limit","resultUnavailable":true})
        } else {
            json!({"source":"runtime","denial":"plan_mode","delivery":"available","isError":true,"resultUnavailable":false})
        };
        send(
            &mut child.0,
            json!({"id":77,"result":{"success":success,"contentItems":[{"type":"inputText","text":"sentinel"},{"type":"inputText","text":receipt.to_string()}]}}),
        );
        assert_eq!(receive()["method"], "item/completed");
        assert_eq!(receive()["method"], "turn/completed");
        let recorded: Value = serde_json::from_slice(&std::fs::read(&proof).unwrap()).unwrap();
        assert_eq!(recorded["success"], success);
        assert_eq!(
            recorded["isError"],
            if case == "unavailable" {
                Value::Null
            } else {
                json!(!success)
            }
        );
        assert_eq!(
            recorded["delivery"],
            if case == "unavailable" {
                "unavailable"
            } else {
                "available"
            }
        );
        assert_eq!(recorded["resultUnavailable"], case == "unavailable");
        assert_eq!(recorded["denialPlanMode"], case == "denied");
        assert_eq!(
            recorded["outcome"],
            if case == "denied" {
                Value::Null
            } else {
                json!("succeeded")
            }
        );
    }
}

#[test]
fn proof_pid_token_is_expanded_only_in_the_direct_child_basename() {
    let state = State::new("proof-pid");
    for _ in 0..2 {
        let mut child = Fixture(
            Command::new(env!("CARGO_BIN_EXE_engram-mcp-model-fixture"))
                .arg("app-server")
                .env_clear()
                .env("ENGRAM_FIXTURE_TOOL", "remote_inventory")
                .env("ENGRAM_FIXTURE_MODE", "read")
                .env("ENGRAM_FIXTURE_STATE_DIR", &state.0)
                .env("ENGRAM_FIXTURE_PROOF", state.0.join("proof-{pid}.json"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let pid = child.0.id();
        assert!(child.0.wait().unwrap().success());
        assert!(state.0.join(format!("proof-{pid}.json")).is_file());
    }
    assert_eq!(std::fs::read_dir(&state.0).unwrap().count(), 2);
    assert!(!state.0.join("proof-{pid}.json").exists());
}

#[path = "support/plan_contract.rs"]
mod plan_contract;
#[path = "support/scalar_contract.rs"]
mod scalar_contract;
