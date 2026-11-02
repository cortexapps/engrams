use super::scalar_contract::{command, finish, hash, native, output, receive};
use super::*;

fn tools() -> Value {
    json!([
        {"name":"spawn_session","description":"Spawn a child session","inputSchema":{"type":"object"}},
        {"name":"mcp_write_counter","description":"Write count","inputSchema":{"type":"object"}},
        {"name":"mcp_read_counter","description":"Read count","inputSchema":{"type":"object"}}
    ])
}
fn plan(claude: bool) -> Value {
    json!({"steps":[
        {"marker":"old","tool":"mcp_write_counter","arguments":{"delta":1},"callId":"write-old","resultFormat":"mcp","expectedResult":native(claude,false)},
        {"marker":"new","tool":"mcp_write_counter","arguments":{"delta":1},"callId":"write-new","resultFormat":"mcp","expectedResult":native(claude,true)},
        {"marker":"spawn","tool":"spawn_session","arguments":{"task_name":"fixture-child","message":"[[fixture:child]]","idempotency_key":"child-once"},"callId":"spawn-once","resultFormat":"json"},
        {"marker":"child","tool":"mcp_read_counter","arguments":{},"callId":"child-read","resultFormat":"mcp","expectedResult":native(claude,false)}
    ]})
}
fn spawn_result(claude: bool) -> Value {
    let text = r#"{"session_id":"neutral-child-id","canonical_task_name":"root/fixture-child"}"#;
    if claude {
        json!({"content":[{"type":"text","text":text}]})
    } else {
        json!({"success":true,"contentItems":[{"type":"inputText","text":text}]})
    }
}
fn plan_command(state: &State, claude: bool, plan: &Value, mode: &str) -> Command {
    let mut cmd = command(state, claude, mode);
    cmd.env_remove("ENGRAM_FIXTURE_TOOL")
        .env("ENGRAM_FIXTURE_CALL_PLAN_JSON", plan.to_string());
    std::fs::write(state.0.join("exposed-tools.json"), tools().to_string()).unwrap();
    cmd
}
fn initialize(
    child: &mut Child,
    rx: &mpsc::Receiver<std::io::Result<String>>,
    claude: bool,
    exposed: Value,
) {
    if claude {
        assert_eq!(receive(rx)["type"], "system");
    } else {
        send(child, json!({"id":1,"method":"initialize"}));
        assert_eq!(receive(rx)["id"], 1);
        send(
            child,
            json!({"id":2,"method":"thread/start","params":{"dynamicTools":exposed}}),
        );
        assert_eq!(receive(rx)["id"], 2);
    }
}
fn prompt(child: &mut Child, claude: bool, text: &str) {
    send(
        child,
        if claude {
            json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":text}]}})
        } else {
            json!({"id":3,"method":"turn/start","params":{"input":[{"type":"text","text":text}]}})
        },
    );
}
fn call(rx: &mpsc::Receiver<std::io::Result<String>>, claude: bool) -> Value {
    if claude {
        receive(rx)["message"]["content"][0].clone()
    } else {
        assert_eq!(receive(rx)["id"], 3);
        receive(rx)["params"].clone()
    }
}
fn complete(
    child: &mut Child,
    rx: &mpsc::Receiver<std::io::Result<String>>,
    claude: bool,
    result: Value,
) {
    if !claude {
        send(child, json!({"id":77,"result":result}));
    }
    let completed = receive(rx);
    assert_eq!(
        if claude {
            &completed["type"]
        } else {
            &completed["method"]
        },
        if claude { "result" } else { "turn/completed" }
    );
}
fn proof(state: &State) -> Vec<Value> {
    std::fs::read_to_string(state.0.join("proof.json"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
#[test]
fn fixed_markers_drive_new_and_repeated_ids_native_spawn_and_child_steps() {
    for claude in [false, true] {
        let state = State::new("plan-turns");
        let plan = plan(claude);
        let response = spawn_result(claude);
        let unknown = json!({"source":"runtime","outcome":"outcome_unknown","delivery":"unavailable","reason":"execution_interrupted","resultUnavailable":false});
        let old_response = if claude {
            json!({"jsonrpc":"2.0","error":{"code":-32603,"message":"External tool output is unavailable.","data":unknown}})
        } else {
            json!({"result":{"success":false,"contentItems":[{"type":"inputText","text":unknown.to_string()}]}})
        };
        let responses = json!({
            "write-old":old_response,
            "write-new":{"jsonrpc":"2.0","result":native(claude,true)},
            "spawn-once":{"jsonrpc":"2.0","result":response},
            "child-read":{"jsonrpc":"2.0","result":native(claude,false)}
        });
        std::fs::write(state.0.join("native-responses.json"), responses.to_string()).unwrap();
        let mut child = Fixture(
            plan_command(&state, claude, &plan, "write")
                .spawn()
                .unwrap(),
        );
        let rx = output(&mut child.0);
        initialize(&mut child.0, &rx, claude, tools());
        for marker in ["old", "old", "new", "spawn", "child"] {
            let step = plan["steps"]
                .as_array()
                .unwrap()
                .iter()
                .find(|step| step["marker"] == marker)
                .unwrap();
            prompt(&mut child.0, claude, &format!("Run [[fixture:{marker}]]"));
            let call = call(&rx, claude);
            assert_eq!(call[if claude { "id" } else { "callId" }], step["callId"]);
            assert_eq!(
                call[if claude { "input" } else { "arguments" }],
                step["arguments"]
            );
            if marker == "child" {
                // The exact fixed child message selects the read step, not spawn.
                assert_eq!(
                    call[if claude { "name" } else { "tool" }],
                    if claude {
                        "mcp__engrams__mcp_read_counter"
                    } else {
                        "mcp_read_counter"
                    }
                );
            }
            complete(
                &mut child.0,
                &rx,
                claude,
                responses[step["callId"].as_str().unwrap()]["result"].clone(),
            );
        }
        finish(&mut child.0);
        let proof = proof(&state);
        assert_eq!(proof.len(), 5);
        for entry in &proof {
            assert_eq!(entry["planSha256"], hash(&plan));
            assert_eq!(
                entry["exposedMcpToolNamesSha256"],
                hash(&json!(["mcp_read_counter", "mcp_write_counter"]))
            );
        }
        assert_eq!(proof[0]["callId"], proof[1]["callId"]);
        assert_eq!(proof[0]["outcome"], "outcome_unknown");
        assert_eq!(proof[0]["delivery"], "unavailable");
        assert!(proof[0].get("resultMatchesExpected").is_none());
        assert_ne!(proof[1]["callId"], proof[2]["callId"]);
        assert_eq!(proof[2]["isError"], true);
        assert_eq!(proof[3]["resultSha256"], hash(&response));
        for key in [
            "resultMatchesExpected",
            "expectedResultSha256",
            "delivery",
            "outcome",
            "isError",
            "resultUnavailable",
        ] {
            assert!(proof[3].get(key).is_none());
        }
        assert!(!std::fs::read_to_string(state.0.join("proof.json"))
            .unwrap()
            .contains("neutral-child-id"));
        let receipt: Value =
            serde_json::from_slice(&std::fs::read(state.0.join("plan-selection.json")).unwrap())
                .unwrap();
        assert_eq!(receipt["pending"], false);
        assert_eq!(receipt["marker"], "child");
    }
}
#[test]
fn plan_rejects_missing_unknown_ambiguous_and_tool_result_markers() {
    for claude in [false, true] {
        for text in [
            "none",
            "[[fixture:unknown]]",
            "[[fixture:old]] [[fixture:new]]",
            "[[fixture:old]] [[fixture:old]]",
            "[[fixture:old",
        ] {
            let state = State::new("invalid-marker");
            let mut child = Fixture(
                plan_command(&state, claude, &plan(claude), "read")
                    .spawn()
                    .unwrap(),
            );
            let rx = output(&mut child.0);
            initialize(&mut child.0, &rx, claude, tools());
            prompt(&mut child.0, claude, text);
            wait_for_failure(&mut child.0);
            assert!(proof(&state).is_empty());
            assert!(!state.0.join("plan-selection.json").exists());
        }
        let state = State::new("tool-result-marker");
        let mut child = Fixture(
            plan_command(&state, claude, &plan(claude), "read")
                .spawn()
                .unwrap(),
        );
        let rx = output(&mut child.0);
        initialize(&mut child.0, &rx, claude, tools());
        send(
            &mut child.0,
            if claude {
                json!({"type":"user","message":{"content":[{"type":"tool_result","content":"[[fixture:old]]"}]}})
            } else {
                json!({"id":3,"method":"turn/start","params":{"input":[{"type":"tool_result","text":"[[fixture:old]]"}]}})
            },
        );
        wait_for_failure(&mut child.0);
        assert!(proof(&state).is_empty());
    }
}
#[test]
fn invalid_plan_configuration_is_rejected_before_any_protocol_output() {
    let valid = plan(false);
    let mut cases = vec![
        json!({"steps":[]}),
        json!({"steps":vec![valid["steps"][0].clone();17]}),
    ];
    for field in ["marker", "tool", "arguments", "callId", "resultFormat"] {
        let mut changed = valid.clone();
        changed["steps"][0].as_object_mut().unwrap().remove(field);
        cases.push(changed);
    }
    for (field, value) in [
        ("extra", json!(true)),
        ("marker", json!("bad/id")),
        ("arguments", json!([])),
        ("resultFormat", json!("guess")),
        ("expectedResult", json!(false)),
    ] {
        let mut changed = valid.clone();
        changed["steps"][0][field] = value;
        cases.push(changed);
    }
    for field in ["marker", "callId"] {
        let mut changed = valid.clone();
        changed["steps"][1][field] = changed["steps"][0][field].clone();
        cases.push(changed);
    }
    for value in cases {
        let state = State::new("invalid-plan");
        let mut child = Fixture(plan_command(&state, false, &value, "read").spawn().unwrap());
        wait_for_failure(&mut child.0);
        assert!(!state.0.join("proof.json").exists());
    }
    let state = State::new("conflicting-plan");
    let mut cmd = plan_command(&state, false, &valid, "read");
    cmd.env("ENGRAM_FIXTURE_CALL_ID", "conflict");
    let mut child = Fixture(cmd.spawn().unwrap());
    wait_for_failure(&mut child.0);
    assert!(!state.0.join("proof.json").exists());
}
#[test]
fn claude_promptless_plan_resume_requires_exact_pending_private_receipt() {
    for case in [
        "missing",
        "complete",
        "changed-plan",
        "wrong-harness",
        "wrong-mode",
        "malformed",
        "symlink",
        "unknown-marker",
        "pending",
    ] {
        let state = State::new("plan-receipt");
        let plan = plan(true);
        let mut receipt = json!({"planSha256":hash(&plan),"harness":"claude","mode":"write","marker":"old","pending":true});
        match case {
            "complete" => receipt["pending"] = json!(false),
            "changed-plan" => receipt["planSha256"] = json!("sha256:other"),
            "wrong-harness" => receipt["harness"] = json!("codex"),
            "wrong-mode" => receipt["mode"] = json!("read"),
            "unknown-marker" => receipt["marker"] = json!("unknown"),
            _ => {}
        }
        let path = state.0.join("plan-selection.json");
        if case == "symlink" {
            std::fs::write(state.0.join("other.json"), receipt.to_string()).unwrap();
            std::os::unix::fs::symlink(state.0.join("other.json"), &path).unwrap();
        } else if case != "missing" {
            std::fs::write(
                &path,
                if case == "malformed" {
                    "{bad".into()
                } else {
                    receipt.to_string()
                },
            )
            .unwrap();
        }
        std::fs::write(
            state.0.join("native-response.json"),
            json!({"jsonrpc":"2.0","result":native(true,false)}).to_string(),
        )
        .unwrap();
        let mut cmd = plan_command(&state, true, &plan, "write");
        cmd.args(["--resume", "fixture-session"]);
        let mut child = Fixture(cmd.spawn().unwrap());
        let rx = output(&mut child.0);
        initialize(&mut child.0, &rx, true, tools());
        if case == "pending" {
            assert_eq!(call(&rx, true)["id"], "write-old");
            complete(&mut child.0, &rx, true, Value::Null);
            finish(&mut child.0);
            assert_eq!(proof(&state)[0]["resultMatchesExpected"], true);
            let receipt: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(receipt["pending"], false);
        } else {
            wait_for_failure(&mut child.0);
            assert!(proof(&state).is_empty());
        }
    }
}

#[test]
fn lost_native_reply_replays_only_the_pending_marker_after_restart() {
    let state = State::new("lost-native-reply");
    let plan = plan(true);
    std::fs::write(state.0.join("hold-result"), b"hold").unwrap();
    let mut child = Fixture(plan_command(&state, true, &plan, "write").spawn().unwrap());
    let rx = output(&mut child.0);
    initialize(&mut child.0, &rx, true, tools());
    prompt(&mut child.0, true, "[[fixture:old]]");
    assert_eq!(call(&rx, true)["id"], "write-old");
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !state.0.join("mock-pending").exists() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    // Closing input aborts the pending receive and reaps the real mock bridge.
    drop(child.0.stdin.take());
    wait_for_failure(&mut child.0);
    assert!(proof(&state).is_empty());
    let receipt: Value =
        serde_json::from_slice(&std::fs::read(state.0.join("plan-selection.json")).unwrap())
            .unwrap();
    assert_eq!(receipt["pending"], true);
    assert_eq!(receipt["marker"], "old");
    assert_eq!(receipt.as_object().unwrap().len(), 5);
    std::fs::remove_file(state.0.join("hold-result")).unwrap();
    std::fs::write(
        state.0.join("native-response.json"),
        json!({"jsonrpc":"2.0","result":native(true,false)}).to_string(),
    )
    .unwrap();
    let mut cmd = plan_command(&state, true, &plan, "write");
    cmd.args(["--resume", "fixture-session"])
        .env("ENGRAM_FIXTURE_PROOF", state.0.join("replayed.json"));
    let mut replay = Fixture(cmd.spawn().unwrap());
    let rx = output(&mut replay.0);
    initialize(&mut replay.0, &rx, true, tools());
    let call = call(&rx, true);
    assert_eq!(call["id"], "write-old");
    assert_eq!(call["input"], json!({"delta":1}));
    complete(&mut replay.0, &rx, true, Value::Null);
    finish(&mut replay.0);
    let receipt: Value =
        serde_json::from_slice(&std::fs::read(state.0.join("plan-selection.json")).unwrap())
            .unwrap();
    assert_eq!(receipt["pending"], false);
    assert_eq!(receipt["marker"], "old");
    let observed: Value =
        serde_json::from_slice(&std::fs::read(state.0.join("replayed.json")).unwrap()).unwrap();
    assert_eq!(observed["resultMatchesExpected"], true);
}

#[test]
fn actual_exposed_mcp_alias_set_is_sorted_bounded_and_validated() {
    for claude in [false, true] {
        for case in ["valid", "extra", "invalid", "duplicate", "over-limit"] {
            let state = State::new("exposure");
            let plan = plan(claude);
            let mut exposed = tools().as_array().unwrap().clone();
            match case {
                "valid"=>exposed.reverse(),
                "extra"=>exposed.push(json!({"name":"mcp_unapproved","description":"extra","inputSchema":{"type":"object"}})),
                "invalid"=>exposed.push(json!({"name":"mcp_bad/name"})),
                "duplicate"=>exposed.push(exposed[1].clone()),
                "over-limit"=>for i in 0..127 {exposed.push(json!({"name":format!("mcp_extra_{i}")}));},
                _=>unreachable!()
            }
            let mut cmd = plan_command(&state, claude, &plan, "read");
            std::fs::write(
                state.0.join("exposed-tools.json"),
                json!(exposed).to_string(),
            )
            .unwrap();
            std::fs::write(
                state.0.join("native-response.json"),
                json!({"jsonrpc":"2.0","result":native(claude,false)}).to_string(),
            )
            .unwrap();
            let mut child = Fixture(cmd.spawn().unwrap());
            let rx = output(&mut child.0);
            if ["invalid", "duplicate", "over-limit"].contains(&case) {
                if !claude {
                    send(&mut child.0, json!({"id":1,"method":"initialize"}));
                    assert_eq!(receive(&rx)["id"], 1);
                    send(
                        &mut child.0,
                        json!({"id":2,"method":"thread/start","params":{"dynamicTools":exposed}}),
                    );
                }
                wait_for_failure(&mut child.0);
                assert!(proof(&state).is_empty());
                continue;
            }
            initialize(&mut child.0, &rx, claude, json!(exposed));
            prompt(&mut child.0, claude, "[[fixture:child]]");
            call(&rx, claude);
            complete(&mut child.0, &rx, claude, native(claude, false));
            finish(&mut child.0);
            let expected = if case == "extra" {
                json!(["mcp_read_counter", "mcp_unapproved", "mcp_write_counter"])
            } else {
                json!(["mcp_read_counter", "mcp_write_counter"])
            };
            assert_eq!(
                proof(&state)[0]["exposedMcpToolNamesSha256"],
                hash(&expected)
            );
        }
    }
}

fn nested_object(depth: usize) -> Value {
    let mut value = json!(0);
    for _ in 0..depth {
        value = json!({"nested":value});
    }
    value
}

#[test]
fn expected_result_nested_native_object_matches_in_scalar_and_plan_modes() {
    // Claude delivers structuredContent as a nested object. Codex delivers it
    // as a literal text string, so it does not have this native nesting depth.
    for depth in [34, 126] {
        let mut actual = native(true, false);
        actual["structuredContent"] = nested_object(depth - 1);
        let scalar = super::scalar_contract::exercise(
            true,
            "read",
            json!({}),
            "nested-read",
            actual.clone(),
            actual.clone(),
            false,
        );
        assert_eq!(scalar["resultMatchesExpected"], true);
        let state = State::new("nested-expected-plan");
        let plan = json!({"steps":[{
            "marker":"nested","tool":"mcp_read_counter","arguments":{},
            "callId":"nested-read","resultFormat":"mcp","expectedResult":actual
        }]});
        std::fs::write(
            state.0.join("native-response.json"),
            json!({"jsonrpc":"2.0","result":actual}).to_string(),
        )
        .unwrap();
        let mut child = Fixture(plan_command(&state, true, &plan, "read").spawn().unwrap());
        let rx = output(&mut child.0);
        initialize(&mut child.0, &rx, true, tools());
        prompt(&mut child.0, true, "[[fixture:nested]]");
        assert_eq!(call(&rx, true)["id"], "nested-read");
        complete(&mut child.0, &rx, true, Value::Null);
        finish(&mut child.0);
        let planned = &proof(&state)[0];
        assert_eq!(planned["resultMatchesExpected"], true);
        assert_eq!(planned["resultSha256"], scalar["resultSha256"]);
        assert_eq!(
            planned["expectedResultSha256"],
            scalar["expectedResultSha256"]
        );
    }
}

fn assert_plan_configuration(plan: &Value, valid: bool) {
    let state = State::new("expected-result-plan-boundary");
    let mut child = Fixture(plan_command(&state, false, plan, "read").spawn().unwrap());
    let rx = output(&mut child.0);
    if valid {
        send(&mut child.0, json!({"id":1,"method":"initialize"}));
        assert_eq!(receive(&rx)["id"], 1);
        finish(&mut child.0);
    } else {
        wait_for_failure(&mut child.0);
        assert!(!state.0.join("proof.json").exists());
        assert!(matches!(
            rx.recv_timeout(Duration::from_secs(3)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }
}

#[test]
fn expected_result_plan_depth_and_byte_boundaries_preserve_argument_limits() {
    let base = json!({"steps":[{
        "marker":"nested","tool":"mcp_read_counter","arguments":{},
        "callId":"nested-read","resultFormat":"mcp","expectedResult":{}
    }]});
    for depth in [127, 128, 129] {
        let expected = nested_object(depth);
        let scalar = super::scalar_contract::config_parser::parse_object(
            &expected.to_string(),
            1024 * 1024,
            128,
        );
        assert_eq!(scalar.is_ok(), depth <= 128);
        let mut plan = base.clone();
        plan["steps"][0]["expectedResult"] = expected;
        assert_plan_configuration(&plan, depth <= 128);
    }
    for depth in [16, 17] {
        let mut plan = base.clone();
        plan["steps"][0]["expectedResult"] = nested_object(128);
        plan["steps"][0]["arguments"] = nested_object(depth);
        assert_plan_configuration(&plan, depth == 16);
    }
    for bytes in [4096, 4097] {
        let mut plan = base.clone();
        plan["steps"][0]["arguments"] = json!({"a":"x".repeat(bytes - 8)});
        assert_eq!(plan["steps"][0]["arguments"].to_string().len(), bytes);
        assert_plan_configuration(&plan, bytes == 4096);
    }
    let mut plan = base;
    plan["steps"][0]["expectedResult"] = json!({"padding":""});
    let padding = 65536 - plan.to_string().len();
    for extra in [0, 1] {
        plan["steps"][0]["expectedResult"]["padding"] = json!("x".repeat(padding + extra));
        assert_eq!(plan.to_string().len(), 65536 + extra);
        assert_plan_configuration(&plan, extra == 0);
    }
}
