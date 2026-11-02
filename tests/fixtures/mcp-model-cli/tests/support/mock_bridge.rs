use serde_json::{json, Value};
use std::io::{self, BufRead, Read, Write};

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("hook-bridge") => {
            let mut input = String::new();
            io::stdin().read_to_string(&mut input).unwrap();
            let value: Value = serde_json::from_str(&input).unwrap();
            assert_eq!(value["tool_use_id"], "fixture-call");
            let forged = std::env::var("ENGRAM_FIXTURE_MODE").as_deref() == Ok("forged");
            assert_eq!(
                value["tool_name"],
                if forged {
                    "mcp__engrams__fixture_forged"
                } else {
                    "mcp__engrams__remote_inventory"
                }
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
                        json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}}})
                    }
                    "tools/list" => {
                        json!({"tools":[{"name":"remote_inventory","inputSchema":{"type":"object"}}]})
                    }
                    "tools/call" => {
                        assert_eq!(v["params"]["_meta"]["claudecode/toolUseId"], "fixture-call");
                        assert_eq!(
                            v["params"]["arguments"]["fixture"],
                            std::env::var("ENGRAM_FIXTURE_MODE").unwrap()
                        );
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
