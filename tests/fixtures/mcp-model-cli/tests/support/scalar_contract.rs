use super::*;
use sha2::{Digest, Sha256};

pub(super) fn hash(value: &Value) -> String {
    // serde_json's default object map is sorted; strings remain literal strings.
    format!("sha256:{:x}", Sha256::digest(value.to_string().as_bytes()))
}
pub(super) fn native(claude: bool, error: bool) -> Value {
    let receipt = json!({"delivery":"available","outcome":if error {"failed"} else {"succeeded"},"resultUnavailable":false,"isError":error});
    if claude {
        json!({"content":[{"type":"text","text":"count=1"},{"type":"text","text":"private-result-marker"}],"structuredContent":{"count":1,"nested":{"z":2,"a":1}},"isError":error,"_meta":{"engrams/toolResult":receipt}})
    } else {
        json!({"success":!error,"contentItems":[{"type":"inputText","text":"count=1"},{"type":"inputText","text":"private-result-marker"},{"type":"inputText","text":"{\"structuredContent\":{\"count\":1,\"nested\":{\"a\":1,\"z\":2}}}"},{"type":"inputText","text":receipt.to_string()}]})
    }
}
pub(super) fn command(state: &State, claude: bool, mode: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_engram-mcp-model-fixture"));
    cmd.env_remove("ENGRAM_FIXTURE_CALL_PLAN_JSON")
        .env_remove("ENGRAM_FIXTURE_ARGUMENTS_JSON")
        .env_remove("ENGRAM_FIXTURE_EXPECTED_RESULT_JSON")
        .env_remove("ENGRAM_FIXTURE_CALL_ID")
        .env_remove("ENGRAM_FIXTURE_BARRIER")
        .env_remove("ENGRAM_FIXTURE_COMPLETION_BARRIER")
        .env_remove("ENGRAM_FIXTURE_PREMATURE_BARRIER")
        .env("ENGRAM_FIXTURE_TOOL", "remote_inventory")
        .env("ENGRAM_FIXTURE_MODE", mode)
        .env("ENGRAM_FIXTURE_STATE_DIR", &state.0)
        .env("ENGRAM_FIXTURE_PROOF", state.0.join("proof.json"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if claude {
        let mock = env!("CARGO_BIN_EXE_fixture-mock-bridge");
        std::fs::write(
            state.0.join("config.json"),
            json!({"mcpServers":{"engrams":{"type":"stdio","command":mock,"args":["mcp-bridge"]}}})
                .to_string(),
        )
        .unwrap();
        std::fs::write(
            state.0.join("settings.json"),
            json!({"hooks":{"PreToolUse":[{"hooks":[{"command":format!("{mock} hook-bridge")}]}]}})
                .to_string(),
        )
        .unwrap();
        cmd.args(["--print", "--strict-mcp-config", "--mcp-config"])
            .arg(state.0.join("config.json"))
            .arg("--settings")
            .arg(state.0.join("settings.json"));
    } else {
        cmd.arg("app-server");
    }
    cmd
}
pub(super) fn output(child: &mut Child) -> mpsc::Receiver<std::io::Result<String>> {
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    rx
}
pub(super) fn receive(rx: &mpsc::Receiver<std::io::Result<String>>) -> Value {
    serde_json::from_str(&rx.recv_timeout(Duration::from_secs(3)).unwrap().unwrap()).unwrap()
}
pub(super) fn finish(child: &mut Child) {
    drop(child.stdin.take());
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(std::time::Instant::now() < deadline, "fixture did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(stderr.is_empty());
}
pub(super) fn exercise(
    claude: bool,
    mode: &str,
    args: Value,
    id: &str,
    actual: Value,
    expected: Value,
    resume: bool,
) -> Value {
    let state = State::new("scalar");
    let mut cmd = command(&state, claude, mode);
    cmd.env("ENGRAM_FIXTURE_ARGUMENTS_JSON", args.to_string())
        .env("ENGRAM_FIXTURE_CALL_ID", id)
        .env("ENGRAM_FIXTURE_EXPECTED_RESULT_JSON", expected.to_string());
    if claude {
        std::fs::write(
            state.0.join("native-response.json"),
            if actual.get("jsonrpc").is_some() {
                actual.clone()
            } else {
                json!({"jsonrpc":"2.0","result":actual})
            }
            .to_string(),
        )
        .unwrap();
        if resume {
            cmd.args(["--resume", "fixture-session"]);
        }
    }
    let mut child = Fixture(cmd.spawn().unwrap());
    let rx = output(&mut child.0);
    if claude {
        assert_eq!(receive(&rx)["type"], "system");
        if !resume {
            send(
                &mut child.0,
                json!({"type":"user","message":{"role":"user","content":"fixture"}}),
            );
        }
        let frame = receive(&rx);
        assert_eq!(frame.pointer("/message/content/0/id"), Some(&json!(id)));
        assert_eq!(frame.pointer("/message/content/0/input"), Some(&args));
        let completion = receive(&rx);
        let block = &completion["message"]["content"][0];
        assert_eq!(completion["type"], "user");
        assert_eq!(block["tool_use_id"], id);
        if actual.get("error").is_some() {
            assert_eq!(block["content"], actual["error"]["message"]);
            assert_eq!(block["is_error"], true);
        } else {
            assert_eq!(block["content"], actual["content"]);
            assert_eq!(block.get("is_error"), actual.get("isError"));
        }
        assert_eq!(receive(&rx)["type"], "result");
    } else {
        send(&mut child.0, json!({"id":1,"method":"initialize"}));
        assert_eq!(receive(&rx)["id"], 1);
        send(
            &mut child.0,
            json!({"id":2,"method":if resume {"thread/resume"} else {"thread/start"},"params":{"threadId":"fixture-thread","dynamicTools":[{"name":"remote_inventory","description":"test","inputSchema":{"type":"object"}}]}}),
        );
        assert_eq!(receive(&rx)["id"], 2);
        send(&mut child.0, json!({"id":3,"method":"turn/start"}));
        assert_eq!(receive(&rx)["id"], 3);
        let call = receive(&rx);
        assert_eq!(call["params"]["callId"], id);
        assert_eq!(call["params"]["arguments"], args);
        send(&mut child.0, json!({"id":77,"result":actual}));
        let completion = receive(&rx);
        assert_eq!(completion["method"], "item/completed");
        assert_eq!(completion["params"]["item"]["id"], id);
        assert_eq!(completion["params"]["item"]["result"], actual);
        assert_eq!(receive(&rx)["method"], "turn/completed");
    }
    finish(&mut child.0);
    let bytes = std::fs::read(state.0.join("proof.json")).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("private-result-marker"));
    assert!(!String::from_utf8_lossy(&bytes).contains("private-argument-marker"));
    serde_json::from_slice(&bytes).unwrap()
}
#[test]
fn actual_counter_arguments_native_results_ids_and_resume_on_both_protocols() {
    for claude in [false, true] {
        for (mode, args, id, resume, error) in [
            ("read", json!({}), "counter-read_1", false, false),
            ("write", json!({"delta":1}), "counter-write_1", false, false),
            ("write", json!({"delta":1}), "counter-write_1", true, false),
            ("write", json!({"delta":1}), "counter-write_2", true, true),
        ] {
            let actual = native(claude, error);
            let proof = exercise(
                claude,
                mode,
                args,
                id,
                actual.clone(),
                actual.clone(),
                resume,
            );
            assert_eq!(proof["callId"], id);
            assert_eq!(proof["selectedToolName"], "remote_inventory");
            assert_eq!(
                proof["selectedSchemaSha256"],
                hash(&json!({"type":"object"}))
            );
            assert_eq!(proof["resultMatchesExpected"], true);
            assert_eq!(proof["resultSha256"], hash(&actual));
            assert_eq!(proof["expectedResultSha256"], hash(&actual));
            assert_eq!(proof["isError"], error);
            assert_eq!(proof["textSentinel"], false);
        }
    }
}
#[test]
fn equality_preserves_array_order_presence_and_literal_codex_strings() {
    for claude in [false, true] {
        for case in 0..4 {
            let actual = native(claude, false);
            let mut expected = actual.clone();
            if claude {
                match case {
                    0 => expected["content"].as_array_mut().unwrap().reverse(),
                    1 => {
                        expected.as_object_mut().unwrap().remove("isError");
                    }
                    2 => expected["structuredContent"]["count"] = json!(2),
                    _ => expected["isError"] = json!(true),
                }
            } else {
                match case {
                    0 => expected["contentItems"].as_array_mut().unwrap().swap(0, 1),
                    1 => {
                        expected
                            .as_object_mut()
                            .unwrap()
                            .insert("absent".into(), Value::Null);
                    }
                    2 => {
                        expected["contentItems"][2]["text"] = json!(
                            "{ \"structuredContent\": {\"count\":1,\"nested\":{\"a\":1,\"z\":2}}}"
                        )
                    }
                    _ => expected["success"] = json!(false),
                }
            }
            let proof = exercise(
                claude,
                "read",
                json!({"marker":"private-argument-marker"}),
                "literal",
                actual.clone(),
                expected.clone(),
                false,
            );
            assert_eq!(proof["resultMatchesExpected"], false);
            assert_eq!(proof["resultSha256"], hash(&actual));
            assert_eq!(proof["expectedResultSha256"], hash(&expected));
        }
    }
}
#[test]
fn invalid_scalar_configuration_fails_closed_without_payload_output() {
    let nested = format!("{}0{}", "{\"a\":".repeat(17), "}".repeat(17));
    let mut cases = vec![
        ("ENGRAM_FIXTURE_ARGUMENTS_JSON", "[1]".into()),
        (
            "ENGRAM_FIXTURE_ARGUMENTS_JSON",
            "{broken-private-marker".into(),
        ),
        ("ENGRAM_FIXTURE_ARGUMENTS_JSON", "{\"a\":1,\"a\":2}".into()),
        (
            "ENGRAM_FIXTURE_ARGUMENTS_JSON",
            "{\"a\":{\"b\":1,\"b\":2}}".into(),
        ),
        ("ENGRAM_FIXTURE_ARGUMENTS_JSON", nested),
        (
            "ENGRAM_FIXTURE_ARGUMENTS_JSON",
            json!({"a":"x".repeat(4096)}).to_string(),
        ),
        ("ENGRAM_FIXTURE_EXPECTED_RESULT_JSON", "null".into()),
        (
            "ENGRAM_FIXTURE_EXPECTED_RESULT_JSON",
            "{\"a\":1,\"a\":2}".into(),
        ),
        ("ENGRAM_FIXTURE_CALL_ID", "".into()),
        ("ENGRAM_FIXTURE_CALL_ID", "a".repeat(65)),
        ("ENGRAM_FIXTURE_CALL_ID", "private-marker/invalid".into()),
        ("ENGRAM_FIXTURE_CALL_ID", "é".into()),
    ];
    cases.push(("ENGRAM_FIXTURE_ARGUMENTS_JSON", "{} {}".into()));
    for claude in [false, true] {
        for (key, value) in &cases {
            let state = State::new("invalid-scalar");
            let mut cmd = command(&state, claude, "read");
            cmd.env(key, value);
            let mut child = Fixture(cmd.spawn().unwrap());
            drop(child.0.stdin.take());
            wait_for_failure(&mut child.0);
            let mut out = String::new();
            std::io::Read::read_to_string(&mut child.0.stdout.take().unwrap(), &mut out).unwrap();
            assert!(out.is_empty());
            let mut err = String::new();
            std::io::Read::read_to_string(&mut child.0.stderr.take().unwrap(), &mut err).unwrap();
            assert_eq!(err, "fixture protocol failure\n");
            assert!(!state.0.join("proof.json").exists());
        }
    }
}

// Compile the exact bounded parser here so the standalone protocol CI target
// covers limits larger than the operating system's environment limit.
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
#[path = "../../src/scalar.rs"]
pub(super) mod config_parser;

#[test]
fn parser_exact_utf8_size_depth_and_canonical_hash_boundaries() {
    assert!(
        config_parser::configured_object("ENGRAM_FIXTURE_TEST_ABSENT_CONFIGURATION", 1, 1)
            .unwrap()
            .is_none()
    );
    for limit in [4096, 1024 * 1024] {
        let raw = format!("{{\"a\":\"{}\"}}", "x".repeat(limit - 8));
        assert_eq!(raw.len(), limit);
        assert!(config_parser::parse_object(&raw, limit, 16).is_ok());
        assert!(config_parser::parse_object(&(raw + " "), limit, 16).is_err());
    }
    let raw = format!("{{\"a\":\"{}\"}}", "é".repeat(2045));
    assert!(raw.chars().count() < 4096);
    assert!(config_parser::parse_object(&raw, 4096, 16).is_err());
    for depth in [16, 17] {
        let raw = format!("{}0{}", "{\"a\":".repeat(depth), "}".repeat(depth));
        assert_eq!(
            config_parser::parse_object(&raw, 4096, 16).is_ok(),
            depth == 16
        );
    }
    let left = config_parser::parse_object(r#"{"z":[{"b":2,"a":1}],"a":0}"#, 4096, 16).unwrap();
    let right = config_parser::parse_object(r#"{ "a":0, "z":[{"a":1,"b":2}]}"#, 4096, 16).unwrap();
    assert_eq!(
        config_parser::digest(&left).unwrap(),
        config_parser::digest(&right).unwrap()
    );
    assert_eq!(
        config_parser::digest(&json!({})).unwrap(),
        "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
    );
}

#[test]
fn unavailable_native_delivery_never_claims_an_expected_output_match() {
    let receipt = json!({"outcome":"succeeded","delivery":"unavailable","resultUnavailable":true,"reason":"unsupported_content"});
    for claude in [false, true] {
        let actual = if claude {
            json!({"jsonrpc":"2.0","error":{"code":-32603,"message":"External tool output is unavailable.","data":receipt}})
        } else {
            json!({"success":false,"contentItems":[{"type":"inputText","text":receipt.to_string()}]})
        };
        let proof = exercise(
            claude,
            "read",
            json!({}),
            "unavailable",
            actual.clone(),
            actual,
            false,
        );
        assert_eq!(proof["delivery"], "unavailable");
        assert_eq!(proof["outcome"], "succeeded");
        assert_eq!(proof["resultUnavailable"], true);
        assert!(proof.get("resultMatchesExpected").is_none());
        assert!(proof.get("resultSha256").is_none());
        assert!(proof.get("expectedResultSha256").is_none());
    }
}

#[test]
fn codex_receipt_comes_from_documented_last_item_not_result_text() {
    let mut actual = native(false, false);
    actual["contentItems"][0]["text"] =
        json!(r#"{"delivery":"unavailable","outcome":"failed","isError":true}"#);
    let proof = exercise(
        false,
        "read",
        json!({}),
        "receipt-position",
        actual.clone(),
        actual,
        false,
    );
    assert_eq!(proof["resultMatchesExpected"], true);
    assert_eq!(proof["delivery"], "available");
    assert_eq!(proof["outcome"], "succeeded");
    assert_eq!(proof["isError"], false);
}

#[test]
fn configured_claude_refire_repeats_id_and_arguments_after_reaped_process() {
    let state = State::new("scalar-refire");
    let actual = native(true, false);
    std::fs::write(
        state.0.join("native-response.json"),
        json!({"jsonrpc":"2.0","result":actual}).to_string(),
    )
    .unwrap();
    for attempt in 0..2 {
        let mut cmd = command(&state, true, "refire");
        let proof = state.0.join(format!("proof-{attempt}.json"));
        cmd.env("ENGRAM_FIXTURE_PROOF", &proof)
            .env("ENGRAM_FIXTURE_ARGUMENTS_JSON", r#"{"delta":1}"#)
            .env("ENGRAM_FIXTURE_CALL_ID", "stable-write_1")
            .env("ENGRAM_FIXTURE_EXPECTED_RESULT_JSON", actual.to_string());
        if attempt == 1 {
            std::fs::write(state.0.join("allow-refire"), b"ready").unwrap();
            cmd.args(["--resume", "fixture-session"]);
        }
        let mut child = Fixture(cmd.spawn().unwrap());
        let rx = output(&mut child.0);
        assert_eq!(receive(&rx)["type"], "system");
        if attempt == 0 {
            send(
                &mut child.0,
                json!({"type":"user","message":{"role":"user","content":"fixture"}}),
            );
        }
        let frame = receive(&rx);
        assert_eq!(
            frame.pointer("/message/content/0/id"),
            Some(&json!("stable-write_1"))
        );
        assert_eq!(
            frame.pointer("/message/content/0/input"),
            Some(&json!({"delta":1}))
        );
        let result = receive(&rx);
        if attempt == 0 {
            assert_eq!(result["terminal_reason"], "tool_deferred");
            assert!(std::fs::read(&proof).unwrap().is_empty());
            // EOF lets the fixture reap its bridge before this process is reaped.
            finish(&mut child.0);
        } else {
            assert_eq!(result["type"], "user");
            assert_eq!(
                result["message"]["content"][0]["tool_use_id"],
                "stable-write_1"
            );
            assert_eq!(receive(&rx)["subtype"], "success");
            finish(&mut child.0);
            let proof: Value = serde_json::from_slice(&std::fs::read(proof).unwrap()).unwrap();
            assert_eq!(proof["callId"], "stable-write_1");
            assert_eq!(proof["resultMatchesExpected"], true);
        }
    }
}
