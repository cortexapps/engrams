//! engrams session … — SessionService passthrough verbs.
//!
//! `create` is the admin escape hatch (raw image URI, allow-all egress —
//! the policy map gates it manage/all); the product-level create is
//! `engrams task create`, which starts from a profile. `logs` tails the
//! orchestrator's SSE route (the gRPC StreamEvents has a dedicated route,
//! not a passthrough); `exec` rides the server-streaming passthrough.

use std::io::Write;

use clap::Args;
use serde::Serialize;

use engram_app_client::proto::*;
use engram_app_client::sse::EventStream;
use engram_app_client::Clients;

use crate::output::{
    compact_json, detail, fail, js_quote, pad_start, print_json, table, truncate, CliError,
    CliResult,
};

#[derive(Serialize)]
struct SessionJson {
    id: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    host_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox_id: Option<String>,
    image: String,
    mode: String,
    created_at: String,
    last_active_at: String,
}

fn session_json(s: Session) -> SessionJson {
    SessionJson {
        id: s.id,
        status: s.status,
        host_id: s.host_id,
        sandbox_id: s.sandbox_id,
        image: s.image,
        mode: s.mode,
        created_at: s.created_at,
        last_active_at: s.last_active_at,
    }
}

pub async fn list(c: &Clients, json: bool) -> CliResult {
    let resp = c.list_sessions(ListSessionsRequest::default()).await?;
    let sessions: Vec<Session> = resp
        .sessions
        .into_iter()
        .filter_map(|i| i.session)
        .collect();
    if json {
        #[derive(Serialize)]
        struct Out {
            sessions: Vec<SessionJson>,
        }
        print_json(&Out {
            sessions: sessions.into_iter().map(session_json).collect(),
        });
        return Ok(());
    }
    if sessions.is_empty() {
        println!("(no sessions)");
        return Ok(());
    }
    table(
        &["ID", "STATUS", "MODE", "IMAGE"],
        &sessions
            .iter()
            .map(|s| {
                vec![
                    s.id.clone(),
                    s.status.clone(),
                    s.mode.clone(),
                    truncate(&s.image, 48),
                ]
            })
            .collect::<Vec<_>>(),
        &[36, 10, 8, 48],
    );
    Ok(())
}

pub async fn get(c: &Clients, id: &str, json: bool) -> CliResult {
    let resp = c
        .get_session(GetSessionRequest {
            session_id: id.to_string(),
        })
        .await?;
    let Some(s) = resp.session else {
        return fail("response carried no session");
    };
    if json {
        print_json(&session_json(s));
        return Ok(());
    }
    detail(&[
        ("id", Some(s.id)),
        ("status", Some(s.status)),
        ("image", Some(s.image)),
        ("mode", Some(s.mode)),
        ("host_id", s.host_id),
        ("sandbox_id", s.sandbox_id),
        ("created_at", Some(s.created_at)),
        ("last_active", Some(s.last_active_at)),
    ]);
    Ok(())
}

#[derive(Args, Debug)]
pub struct CreateOpts {
    /// image to boot (flat OCI URI; must be enabled)
    #[arg(long, value_name = "uri")]
    pub image: String,
    /// pure dev VM — no harness driven; interact via exec
    #[arg(long = "dev-vm")]
    pub dev_vm: bool,
    /// catalog harness for an agent session (e.g. claude)
    #[arg(long, value_name = "name")]
    pub harness: Option<String>,
    /// initial prompt (agent mode only)
    #[arg(long, value_name = "text")]
    pub prompt: Option<String>,
}

pub async fn create(c: &Clients, opts: CreateOpts, json: bool) -> CliResult {
    let resp = c
        .create_session(CreateSessionRequest {
            image_uri: opts.image,
            mode: if opts.dev_vm { "dev_vm" } else { "agent" }.to_string(),
            prompt: opts.prompt,
            harness: opts.harness,
            // Admin debug create: allow-all egress (no profile to compile a
            // policy from) — same posture as the CLIs before it.
            integration_policy_json: r#"{"network":{"default":"allow"}}"#.to_string(),
            ..Default::default()
        })
        .await?;
    if json {
        #[derive(Serialize)]
        struct Out {
            session_id: String,
            status: String,
            image_version: String,
            kind: String,
        }
        print_json(&Out {
            session_id: resp.session_id,
            status: resp.status,
            image_version: resp.image_version,
            kind: resp.kind,
        });
    } else {
        println!("{}", resp.session_id);
    }
    Ok(())
}

pub async fn remove(c: &Clients, id: &str) -> CliResult {
    c.delete_session(DeleteSessionRequest {
        session_id: id.to_string(),
    })
    .await?;
    println!("deleted");
    Ok(())
}

pub async fn exec(
    c: &Clients,
    id: &str,
    cmd: &str,
    timeout_secs: Option<u64>,
    json: bool,
) -> CliResult {
    let mut stream = c
        .exec(ExecRequest {
            session_id: id.to_string(),
            command: Some(cmd.to_string()),
            timeout_secs,
            ..Default::default()
        })
        .await?;
    let mut out: Vec<u8> = Vec::new();
    let mut err_buf: Vec<u8> = Vec::new();
    let mut exit_status: Option<i32> = None;
    let mut saw_exit = false;
    while let Some(item) = stream.next().await {
        let msg = item?;
        match msg.event {
            Some(exec_output::Event::Stdout(b)) => {
                if json {
                    out.extend_from_slice(&b);
                } else {
                    let mut so = std::io::stdout().lock();
                    so.write_all(&b)?;
                    so.flush()?;
                }
            }
            Some(exec_output::Event::Stderr(b)) => {
                if json {
                    err_buf.extend_from_slice(&b);
                } else {
                    let mut se = std::io::stderr().lock();
                    se.write_all(&b)?;
                    se.flush()?;
                }
            }
            Some(exec_output::Event::Exit(e)) => {
                saw_exit = true;
                exit_status = e.exit_status;
            }
            _ => {}
        }
    }
    if json {
        #[derive(Serialize)]
        struct Out {
            stdout: String,
            stderr: String,
            exit_status: Option<i32>,
        }
        print_json(&Out {
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err_buf).into_owned(),
            exit_status: if saw_exit { exit_status } else { None },
        });
    }
    // Mirror the remote exit so shell pipelines compose (killed = failure).
    if exit_status != Some(0) {
        return Err(CliError::Exit(1));
    }
    Ok(())
}

/// Render one event envelope: `[{idx}] {kind}: {payload}`.
pub fn format_event_line(idx: Option<&str>, kind: &str, payload_json: &str) -> String {
    let payload = compact_json(payload_json).unwrap_or_else(|| js_quote(payload_json));
    format!("[{}] {kind}: {payload}", pad_start(idx.unwrap_or("-"), 6))
}

/// Tail the persistent event log via the orchestrator's SSE route
/// (`GET /api/v1/sessions/:id/events`). Stays open; Ctrl-C to stop.
pub async fn logs(c: &Clients, id: &str, since: Option<i64>) -> CliResult {
    let mut stream = match EventStream::open(&c.transport, id, since, None).await {
        Ok(s) => s,
        // The fetch/HTTP failure message, with no code prefix, as before.
        Err(e) => return fail(e.message),
    };
    // Skip the 15s `ping` keepalives; print everything else.
    while let Some(frame) = stream
        .next()
        .await
        .map_err(|e| CliError::Message(e.message))?
    {
        match frame.event.as_deref() {
            Some("ping") | None => continue,
            Some(kind) => {
                println!(
                    "{}",
                    format_event_line(frame.id.as_deref(), kind, &frame.data.join("\n"))
                )
            }
        }
    }
    Ok(())
}

pub async fn log(c: &Clients, id: &str, limit: Option<i64>, tail: bool, json: bool) -> CliResult {
    let resp = c
        .get_log(GetLogRequest {
            session_id: id.to_string(),
            kind: Some("conversation".to_string()),
            tail: Some(tail),
            limit,
        })
        .await?;
    if json {
        #[derive(Serialize)]
        struct Ev {
            idx: i64,
            kind: String,
            at: String,
            payload: serde_json::Value,
        }
        #[derive(Serialize)]
        struct Out {
            session_id: String,
            kind: String,
            events: Vec<Ev>,
        }
        print_json(&Out {
            session_id: resp.session_id,
            kind: resp.kind,
            events: resp
                .events
                .into_iter()
                .map(|e| Ev {
                    idx: e.idx,
                    kind: e.kind,
                    at: e.at,
                    payload: parse_or(&e.payload_json),
                })
                .collect(),
        });
        return Ok(());
    }
    if resp.events.is_empty() {
        println!("(no events)");
        return Ok(());
    }
    table(
        &["IDX", "KIND", "AT", "PAYLOAD"],
        &resp
            .events
            .iter()
            .map(|e| {
                vec![
                    e.idx.to_string(),
                    truncate(&e.kind, 28),
                    e.at.clone(),
                    truncate(&summarize(&e.payload_json), 80),
                ]
            })
            .collect::<Vec<_>>(),
        &[6, 28, 25, 80],
    );
    Ok(())
}

/// `JSON.parse` with the raw string as the fallback.
fn parse_or(payload_json: &str) -> serde_json::Value {
    serde_json::from_str(payload_json)
        .unwrap_or_else(|_| serde_json::Value::String(payload_json.to_string()))
}

/// The top level of a JSON document with object keys in document order
/// (the summary shows the FIRST three entries, as `Object.entries` did).
enum Top {
    Null,
    Str(String),
    Scalar(String),
    Array(usize),
    Object(Vec<(String, serde_json::Value)>),
}

impl<'de> serde::Deserialize<'de> for Top {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Top;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_bool<E>(self, v: bool) -> Result<Top, E> {
                Ok(Top::Scalar(v.to_string()))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Top, E> {
                Ok(Top::Scalar(v.to_string()))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Top, E> {
                Ok(Top::Scalar(v.to_string()))
            }
            fn visit_f64<E>(self, v: f64) -> Result<Top, E> {
                Ok(Top::Scalar(v.to_string()))
            }
            fn visit_str<E>(self, v: &str) -> Result<Top, E> {
                Ok(Top::Str(v.to_string()))
            }
            fn visit_unit<E>(self) -> Result<Top, E> {
                Ok(Top::Null)
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Top, A::Error> {
                let mut n = 0;
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                    n += 1;
                }
                Ok(Top::Array(n))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Top, A::Error> {
                let mut entries = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, serde_json::Value>()? {
                    entries.push((k, v));
                }
                Ok(Top::Object(entries))
            }
        }
        d.deserialize_any(V)
    }
}

fn summarize(payload_json: &str) -> String {
    match serde_json::from_str::<Top>(payload_json) {
        Err(_) => truncate(payload_json, 24),
        Ok(Top::Null) => String::new(),
        Ok(Top::Str(s)) => truncate(&s, 24),
        Ok(Top::Scalar(s)) => s,
        Ok(Top::Array(n)) => format!("[{n} items]"),
        Ok(Top::Object(entries)) => entries
            .iter()
            .take(3)
            .map(|(k, v)| format!("{k}={}", short_value(v)))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

fn short_value(v: &serde_json::Value) -> String {
    use serde_json::Value::*;
    match v {
        String(s) => truncate(s, 24),
        Null => "null".to_string(),
        Array(a) => format!("[{} items]", a.len()),
        Object(o) => format!("{{{} keys}}", o.len()),
        Bool(b) => b.to_string(),
        Number(n) => n.to_string(),
    }
}

pub async fn resume(c: &Clients, id: &str, json: bool) -> CliResult {
    let resp = c
        .resume(ResumeRequest {
            session_id: id.to_string(),
        })
        .await?;
    if json {
        #[derive(Serialize)]
        struct Out {
            session_id: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            snapshot_id: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            size_bytes: Option<u64>,
            note: String,
        }
        print_json(&Out {
            session_id: resp.session_id,
            snapshot_id: resp.snapshot_id,
            size_bytes: resp.size_bytes,
            note: resp.note,
        });
        return Ok(());
    }
    println!(
        "{}",
        if resp.note.is_empty() {
            "resumed"
        } else {
            &resp.note
        }
    );
    Ok(())
}

pub async fn prompt(c: &Clients, id: &str, text: &str, json: bool) -> CliResult {
    let resp = c
        .send_prompt(SendPromptRequest {
            session_id: id.to_string(),
            text: text.to_string(),
            prompt_id: String::new(),
            harness_mode: None,
        })
        .await?;
    if json {
        #[derive(Serialize)]
        struct Out {
            session_id: String,
            note: String,
        }
        print_json(&Out {
            session_id: resp.session_id,
            note: resp.note,
        });
    } else {
        println!("prompt forwarded");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_event_line_matches_the_line_contract() {
        let out = format_event_line(
            Some("7"),
            "exec_completed",
            r#"{"exec_id":"x","exit_status":0}"#,
        );
        assert_eq!(
            out,
            r#"[     7] exec_completed: {"exec_id":"x","exit_status":0}"#
        );
        // idx-less lagged frames render `-`; non-JSON payloads become strings.
        assert_eq!(
            format_event_line(None, "lagged", r#"{"missed":3}"#),
            r#"[     -] lagged: {"missed":3}"#
        );
        assert_eq!(
            format_event_line(Some("1"), "raw", "not-json"),
            r#"[     1] raw: "not-json""#
        );
    }

    #[test]
    fn summarize_shows_the_first_three_entries_in_document_order() {
        assert_eq!(
            summarize(r#"{"z":1,"y":"a long string value here!!","x":[1,2],"w":0}"#),
            "z=1 y=a long string value her… x=[2 items]"
        );
        assert_eq!(summarize("[1,2,3]"), "[3 items]");
        assert_eq!(summarize("null"), "");
        assert_eq!(summarize("\"s\""), "s");
        assert_eq!(summarize("true"), "true");
        assert_eq!(summarize("junk"), "junk");
    }
}
