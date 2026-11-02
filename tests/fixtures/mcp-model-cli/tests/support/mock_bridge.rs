#[path = "../../src/scalar.rs"]
mod scalar;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

use serde_json::{json, Value};
use std::io::{self, BufRead, Read, Write};

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("hook-bridge") => {
            let mut input = String::new();
            io::stdin().read_to_string(&mut input).unwrap();
            let value: Value = serde_json::from_str(&input).unwrap();
            assert!(value["tool_use_id"] == call_id());
            assert!(value["tool_input"] == arguments());
            let forged = std::env::var("ENGRAM_FIXTURE_MODE").as_deref() == Ok("forged");
            assert!(
                value["tool_name"]
                    == format!(
                        "mcp__engrams__{}",
                        if forged {
                            "fixture_forged".to_owned()
                        } else {
                            tool()
                        }
                    )
            );
            let deferred = std::env::var("ENGRAM_FIXTURE_MODE").as_deref() == Ok("refire")
                && !std::path::Path::new(&std::env::var_os("ENGRAM_FIXTURE_STATE_DIR").unwrap())
                    .join("allow-refire")
                    .exists();
            println!(
                "{}",
                json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":if deferred { "defer" } else { "allow" }}})
            );
        }
        Some("mcp-bridge") => {
            for line in io::stdin().lock().lines() {
                let v: Value = serde_json::from_str(&line.unwrap()).unwrap();
                let id = v["id"].clone();
                if v["method"] == "initialize" {
                    let root = std::env::var_os("ENGRAM_FIXTURE_STATE_DIR").unwrap();
                    let marker = std::path::Path::new(&root).join("bad-initialize");
                    if let Ok(case) = std::fs::read_to_string(marker) {
                        let response = match case.as_str() {
                            "error" => {
                                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"fixture error"}})
                            }
                            "missing" => json!({"jsonrpc":"2.0","id":id}),
                            "malformed" => {
                                json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-06-18","capabilities":[],"serverInfo":{"name":"fixture","version":"1"}}})
                            }
                            "server-info" => {
                                json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"","version":"1"}}})
                            }
                            "version" => {
                                json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":"unsupported","capabilities":{},"serverInfo":{"name":"fixture","version":"1"}}})
                            }
                            "both" => {
                                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"fixture error"},"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"fixture","version":"1"}}})
                            }
                            _ => panic!("invalid case"),
                        };
                        println!("{response}");
                        io::stdout().flush().unwrap();
                        continue;
                    }
                }
                if v["method"] == "tools/call" && v["params"]["name"] == "fixture_forged" {
                    println!(
                        "{}",
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"Unknown tool"}})
                    );
                    io::stdout().flush().unwrap();
                    continue;
                }
                let result = match v["method"].as_str().unwrap() {
                    "initialize" => {
                        json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
                    }
                    "tools/list" => {
                        let root = std::env::var_os("ENGRAM_FIXTURE_STATE_DIR").unwrap();
                        if let Ok(raw) =
                            std::fs::read(std::path::Path::new(&root).join("exposed-tools.json"))
                        {
                            json!({"tools":serde_json::from_slice::<Value>(&raw).unwrap()})
                        } else {
                            json!({"tools":[{"name":"remote_inventory","inputSchema":{"type":"object"}}]})
                        }
                    }
                    "tools/call" => {
                        assert!(v["params"]["_meta"]["claudecode/toolUseId"] == call_id());
                        assert!(v["params"]["arguments"] == arguments());
                        let root = std::env::var_os("ENGRAM_FIXTURE_STATE_DIR").unwrap();
                        if std::path::Path::new(&root).join("hold-result").exists() {
                            std::fs::write(
                                std::path::Path::new(&root).join("mock-pending"),
                                b"ready",
                            )
                            .unwrap();
                            continue;
                        }
                        if let Ok(raw) =
                            std::fs::read(std::path::Path::new(&root).join("native-responses.json"))
                        {
                            let responses: Value = serde_json::from_slice(&raw).unwrap();
                            let mut response = responses.get(call_id()).unwrap().clone();
                            response["id"] = id;
                            println!("{response}");
                            io::stdout().flush().unwrap();
                            continue;
                        }
                        if let Ok(raw) =
                            std::fs::read(std::path::Path::new(&root).join("native-response.json"))
                        {
                            let mut response: Value = serde_json::from_slice(&raw).unwrap();
                            response["id"] = id;
                            println!("{response}");
                            io::stdout().flush().unwrap();
                            continue;
                        }
                        if std::env::var("ENGRAM_FIXTURE_MODE").as_deref() == Ok("held-call") {
                            let root = std::env::var_os("ENGRAM_FIXTURE_STATE_DIR").unwrap();
                            std::fs::write(
                                std::path::Path::new(&root).join("mock-pending"),
                                b"ready",
                            )
                            .unwrap();
                            continue;
                        }
                        if std::env::var("ENGRAM_FIXTURE_MODE").as_deref() == Ok("denied") {
                            json!({"content":[{"type":"text","text":"sentinel"},{"type":"text","text":"second sentinel"}],"structuredContent":{"fixture":true},"isError":true,"_meta":{"engrams/toolResult":{"source":"runtime","denial":"plan_mode","delivery":"available","resultUnavailable":false,"isError":true}}})
                        } else {
                            json!({"content":[{"type":"text","text":"sentinel"},{"type":"text","text":"second sentinel"}],"structuredContent":{"fixture":true},"isError":false,"_meta":{"engrams/toolResult":{"outcome":"succeeded","delivery":"available","resultUnavailable":false,"isError":false}}})
                        }
                    }
                    "notifications/initialized" => continue,
                    "notifications/cancelled" => continue,
                    _ => panic!("unexpected method"),
                };
                println!("{}", json!({"jsonrpc":"2.0","id":id,"result":result}));
                io::stdout().flush().unwrap();
            }
        }
        _ => std::process::exit(1),
    }
}

fn active_step() -> Option<Value> {
    let plan =
        scalar::configured_object("ENGRAM_FIXTURE_CALL_PLAN_JSON", 65536, 128 + 3).unwrap()?;
    let root = std::env::var_os("ENGRAM_FIXTURE_STATE_DIR").unwrap();
    let receipt: Value = serde_json::from_slice(
        &std::fs::read(std::path::Path::new(&root).join("plan-selection.json")).unwrap(),
    )
    .unwrap();
    assert!(receipt["planSha256"] == scalar::digest(&plan).unwrap());
    Some(
        plan["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["marker"] == receipt["marker"])
            .unwrap()
            .clone(),
    )
}
fn call_id() -> String {
    active_step()
        .map(|step| step["callId"].as_str().unwrap().to_owned())
        .unwrap_or_else(|| {
            std::env::var("ENGRAM_FIXTURE_CALL_ID").unwrap_or_else(|_| "fixture-call".into())
        })
}
fn tool() -> String {
    active_step()
        .map(|step| step["tool"].as_str().unwrap().to_owned())
        .unwrap_or_else(|| std::env::var("ENGRAM_FIXTURE_TOOL").unwrap())
}
fn arguments() -> Value {
    active_step()
        .map(|step| step["arguments"].clone())
        .unwrap_or_else(|| {
            std::env::var("ENGRAM_FIXTURE_ARGUMENTS_JSON")
                .map(|raw| serde_json::from_str(&raw).unwrap())
                .unwrap_or_else(
                    |_| json!({"fixture":std::env::var("ENGRAM_FIXTURE_MODE").unwrap()}),
                )
        })
}
