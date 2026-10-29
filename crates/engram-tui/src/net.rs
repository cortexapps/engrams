//! The network side: one task per attached session (status, the history
//! tail, then the SSE loop with reconnect) and fire-and-forget RPCs for
//! actions. Everything reports back through one channel as `NetMsg`.

use std::time::Duration;

use engram_app_client::proto::*;
use engram_app_client::shell::{Shell, ShellMsg};
use engram_app_client::sse::EventStream;
use engram_app_client::{Clients, Code, Error};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::app::{Msg, NetMsg, PAGE};
use crate::event::Envelope;
use crate::sessions::{rows_from_profiles, rows_from_tasks, RunState};

pub struct Net {
    clients: Clients,
    tx: UnboundedSender<Msg>,
    attach: Option<JoinHandle<()>>,
    shell: Option<ShellLink>,
}

/// The open shell: its task and the channel that feeds it keystrokes.
struct ShellLink {
    task: JoinHandle<()>,
    tx: UnboundedSender<ShellOut>,
}

enum ShellOut {
    Input(Vec<u8>),
    Resize(u16, u16),
}

/// Active sessions asked per poll whether a run is in progress.
const MAX_RUN_CHECKS: usize = 16;

/// The run events, newest first: a start (or continuation) without a
/// later end is a run in progress.
const RUN_KINDS: [&str; 4] = [
    "run_started",
    "run_continued",
    "run_completed",
    "run_interrupted",
];

/// The session list's deadline, and each run read's.
const LIST_TIMEOUT: Duration = Duration::from_secs(10);
const RUN_TIMEOUT: Duration = Duration::from_secs(5);

/// What `session_id`'s newest run event says. One event read. `None` when
/// the read failed: unknown, not stopped.
async fn run_state(c: &Clients, session_id: &str) -> Option<RunState> {
    let page = c
        .list_session_events(ListSessionEventsRequest {
            session_id: session_id.to_string(),
            after_idx: None,
            limit: Some(1),
            before_idx: Some(i64::MAX),
            kinds: RUN_KINDS.iter().map(|k| k.to_string()).collect(),
            tool_names: Vec::new(),
        })
        .await
        .ok()?;
    let Some(e) = page.events.last() else {
        return Some(RunState::NoRun);
    };
    Some(match e.kind.as_str() {
        "run_started" | "run_continued" => RunState::Running,
        "run_interrupted" => RunState::Interrupted,
        _ => RunState::Ended {
            ok: serde_json::from_str::<serde_json::Value>(&e.payload_json)
                .ok()
                .and_then(|v| v.get("ok").and_then(serde_json::Value::as_bool))
                .unwrap_or(true),
        },
    })
}

/// An RPC error as the line the app shows. A refused API key also tells
/// the app, which reports that the user has to sign in.
fn describe(tx: &UnboundedSender<Msg>, e: &Error) -> String {
    if e.code == Code::Unauthenticated {
        let _ = tx.send(Msg::Net(NetMsg::AuthFailed));
    }
    e.to_string()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Net {
    pub fn new(clients: Clients, tx: UnboundedSender<Msg>) -> Self {
        Net {
            clients,
            tx,
            attach: None,
            shell: None,
        }
    }

    pub fn load_sessions(&self) {
        let c = self.clients.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            // Your own tasks, admins included: the list is yours, and the
            // terminal's records say what waits on you, not on others.
            let list = c.list_tasks(ListTasksRequest {
                scope: "mine".into(),
                page_size: 200,
                ..Default::default()
            });
            // A stalled call must not hold the next poll back forever.
            let res = match tokio::time::timeout(LIST_TIMEOUT, list).await {
                Ok(res) => res
                    .map(|r| {
                        let known = !r.session_state_unavailable;
                        (rows_from_tasks(&r.tasks, now_unix(), known), known)
                    })
                    .map_err(|e| describe(&tx, &e)),
                Err(_) => Err("the session list timed out".to_string()),
            };
            let known = matches!(res, Ok((_, true)));
            let res = res.map(|(rows, _)| rows);
            // The list shows now; which active sessions are running
            // follows.
            let active: Vec<String> = match &res {
                Ok(rows) => rows
                    .iter()
                    .filter(|r| r.status == "active")
                    .take(MAX_RUN_CHECKS)
                    .map(|r| r.session_id.clone())
                    .collect(),
                Err(_) => Vec::new(),
            };
            let ok = res.is_ok();
            let _ = tx.send(Msg::Net(NetMsg::Sessions(res)));
            if !ok {
                return;
            }
            let checks = active.into_iter().map(|id| {
                let c = c.clone();
                async move {
                    let run = tokio::time::timeout(RUN_TIMEOUT, run_state(&c, &id))
                        .await
                        .ok()
                        .flatten();
                    run.map(|run| (id, run))
                }
            });
            let running = futures::future::join_all(checks)
                .await
                .into_iter()
                .flatten()
                .collect();
            let _ = tx.send(Msg::Net(NetMsg::SessionsRunning { running, known }));
        });
    }

    pub fn load_profiles(&self) {
        let c = self.clients.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = c
                .list_profiles(ListProfilesRequest {
                    include_archived: false,
                    for_session_picker: true,
                })
                .await
                .map(|r| rows_from_profiles(&r.profiles))
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Net(NetMsg::Profiles(res)));
        });
    }

    /// `CreateTask` with the prompt; answers with the primary session's id
    /// and the task's title (the prompt's first line when the server set
    /// none).
    pub fn create_task(&self, profile_id: String, prompt: String) {
        let c = self.clients.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let fallback = prompt.lines().next().unwrap_or_default().to_string();
            let res = c
                .create_task(CreateTaskRequest {
                    r#type: "chat".into(),
                    prompt: Some(prompt),
                    profile_id,
                    ..Default::default()
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| {
                    let task = r
                        .task
                        .ok_or_else(|| "the server returned no task".to_string())?;
                    let session_id = task
                        .sessions
                        .first()
                        .map(|s| s.session_id.clone())
                        .ok_or_else(|| "the task has no session".to_string())?;
                    let title = task.title.filter(|t| !t.is_empty()).unwrap_or(fallback);
                    Ok((session_id, title))
                });
            let _ = tx.send(Msg::Net(NetMsg::Created(res)));
        });
    }

    pub fn attach(&mut self, session_id: String) {
        self.detach();
        let c = self.clients.clone();
        let tx = self.tx.clone();
        self.attach = Some(tokio::spawn(async move {
            attach_task(c, tx, session_id).await;
        }));
    }

    pub fn detach(&mut self) {
        self.shell_close();
        if let Some(h) = self.attach.take() {
            h.abort();
        }
    }

    /// Dial the session shell; one at a time.
    pub fn shell_open(&mut self, session_id: String, cols: u16, rows: u16) {
        self.shell_close();
        let (stx, srx) = mpsc::unbounded_channel();
        let c = self.clients.clone();
        let tx = self.tx.clone();
        let task = tokio::spawn(async move {
            shell_task(c, tx, session_id, cols, rows, srx).await;
        });
        self.shell = Some(ShellLink { task, tx: stx });
    }

    pub fn shell_input(&self, bytes: Vec<u8>) {
        if let Some(s) = &self.shell {
            let _ = s.tx.send(ShellOut::Input(bytes));
        }
    }

    pub fn shell_resize(&self, cols: u16, rows: u16) {
        if let Some(s) = &self.shell {
            let _ = s.tx.send(ShellOut::Resize(cols, rows));
        }
    }

    /// Drop the socket; the server ends the PTY.
    pub fn shell_close(&mut self) {
        if let Some(s) = self.shell.take() {
            s.task.abort();
        }
    }

    pub fn load_older(&self, session_id: String, before_idx: i64) {
        let c = self.clients.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match list_page(&c, &tx, &session_id, before_idx).await {
                Ok(events) => {
                    let _ = tx.send(Msg::Net(NetMsg::History {
                        session_id,
                        events,
                        older: true,
                    }));
                }
                Err(error) => {
                    let _ = tx.send(Msg::Net(NetMsg::OlderFailed { session_id, error }));
                }
            }
        });
    }

    pub fn send_prompt(
        &self,
        session_id: String,
        prompt_id: String,
        text: String,
        harness_mode: Option<String>,
    ) {
        let c = self.clients.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = c
                .send_prompt(SendPromptRequest {
                    session_id: session_id.clone(),
                    text,
                    prompt_id: prompt_id.clone(),
                    harness_mode,
                })
                .await;
            if let Err(e) = res {
                let _ = tx.send(Msg::Net(NetMsg::SendFailed {
                    session_id,
                    prompt_id,
                    error: e.to_string(),
                }));
            }
        });
    }

    /// Answer a deferred tool call (a question, a plan decision).
    pub fn complete_tool_call(
        &self,
        session_id: String,
        tool_call_id: String,
        result_json: String,
    ) {
        let c = self.clients.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = c
                .complete_tool_call(CompleteToolCallRequest {
                    session_id: session_id.clone(),
                    tool_call_id: tool_call_id.clone(),
                    result_json,
                })
                .await;
            if let Err(e) = res {
                let _ = tx.send(Msg::Net(NetMsg::CompleteFailed {
                    session_id,
                    tool_call_id,
                    error: e.to_string(),
                }));
            }
        });
    }

    pub fn interrupt(&self, session_id: String) {
        let c = self.clients.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = c
                .interrupt(InterruptRequest {
                    session_id: session_id.clone(),
                    source: "tui".into(),
                })
                .await;
            if let Err(e) = res {
                let _ = tx.send(Msg::Net(NetMsg::Failed {
                    session_id,
                    error: format!("interrupt failed: {e}"),
                }));
            }
        });
    }

    pub fn resume(&self, session_id: String) {
        let c = self.clients.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = c
                .resume(ResumeRequest {
                    session_id: session_id.clone(),
                })
                .await;
            if let Err(e) = res {
                let _ = tx.send(Msg::Net(NetMsg::Failed {
                    session_id,
                    error: format!("resume failed: {e}"),
                }));
            }
        });
    }

    pub fn dequeue(&self, session_id: String, prompt_id: String) {
        let c = self.clients.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = c
                .dequeue_queued_prompt(DequeueQueuedPromptRequest {
                    session_id: session_id.clone(),
                    prompt_id,
                })
                .await;
            if let Err(e) = res {
                let _ = tx.send(Msg::Net(NetMsg::Failed {
                    session_id,
                    error: format!("dequeue failed: {e}"),
                }));
            }
        });
    }
}

fn to_envelopes(events: Vec<SessionEvent>) -> Vec<Envelope> {
    events
        .into_iter()
        .map(|e| Envelope {
            idx: e.idx,
            kind: e.kind,
            payload_json: e.payload_json,
        })
        .collect()
}

async fn list_page(
    c: &Clients,
    tx: &UnboundedSender<Msg>,
    session_id: &str,
    before_idx: i64,
) -> Result<Vec<Envelope>, String> {
    c.list_session_events(ListSessionEventsRequest {
        session_id: session_id.to_string(),
        after_idx: None,
        limit: Some(PAGE),
        before_idx: Some(before_idx),
        kinds: Vec::new(),
        tool_names: Vec::new(),
    })
    .await
    .map(|r| to_envelopes(r.events))
    .map_err(|e| describe(tx, &e))
}

async fn attach_task(c: Clients, tx: UnboundedSender<Msg>, session_id: String) {
    let send = |m: NetMsg| {
        let _ = tx.send(Msg::Net(m));
    };
    match c
        .get_session(GetSessionRequest {
            session_id: session_id.clone(),
        })
        .await
    {
        Ok(r) => {
            if let Some(s) = r.session {
                send(NetMsg::Status {
                    session_id: session_id.clone(),
                    status: s.status,
                    title: s.suggested_title,
                });
            }
        }
        Err(e) => send(NetMsg::Failed {
            session_id: session_id.clone(),
            error: describe(&tx, &e),
        }),
    }
    let mut since: Option<i64> = None;
    match list_page(&c, &tx, &session_id, i64::MAX).await {
        Ok(events) => {
            since = events.iter().filter_map(|e| e.idx).max();
            send(NetMsg::History {
                session_id: session_id.clone(),
                events,
                older: false,
            });
        }
        Err(error) => send(NetMsg::Failed {
            session_id: session_id.clone(),
            error,
        }),
    }

    let mut attempt: u32 = 0;
    loop {
        let stream = EventStream::open(&c.transport, &session_id, since, since).await;
        let mut stream = match stream {
            Ok(s) => s,
            Err(e) => {
                attempt += 1;
                send(NetMsg::Reconnecting {
                    session_id: session_id.clone(),
                    attempt,
                });
                send(NetMsg::Failed {
                    session_id: session_id.clone(),
                    error: describe(&tx, &e),
                });
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
        };
        send(NetMsg::Connected {
            session_id: session_id.clone(),
        });
        attempt = 0;
        loop {
            match stream.next().await {
                Ok(Some(frame)) => {
                    if frame.event.as_deref() == Some("ping") || frame.data.is_empty() {
                        continue;
                    }
                    let data = frame.data.join("\n");
                    let Ok(env) = serde_json::from_str::<Envelope>(&data) else {
                        continue; // a lagged frame or an unknown shape
                    };
                    if let Some(idx) = env.idx {
                        since = Some(since.map_or(idx, |s| s.max(idx)));
                    }
                    send(NetMsg::Live {
                        session_id: session_id.clone(),
                        envelope: env,
                    });
                }
                Ok(None) => break,
                Err(e) => {
                    send(NetMsg::Failed {
                        session_id: session_id.clone(),
                        error: e.to_string(),
                    });
                    break;
                }
            }
        }
        attempt += 1;
        send(NetMsg::Reconnecting {
            session_id: session_id.clone(),
            attempt,
        });
        tokio::time::sleep(backoff(attempt)).await;
    }
}

/// Pump the shell: frames in become messages, keystrokes out go on the
/// socket. Ends with one `ShellEnded`.
async fn shell_task(
    c: Clients,
    tx: UnboundedSender<Msg>,
    session_id: String,
    cols: u16,
    rows: u16,
    mut out: UnboundedReceiver<ShellOut>,
) {
    let send = |m: NetMsg| {
        let _ = tx.send(Msg::Net(m));
    };
    let ended = |error: Option<String>| NetMsg::ShellEnded {
        session_id: session_id.clone(),
        error,
    };
    let mut shell = match Shell::open(&c.transport, &session_id, cols, rows).await {
        Ok(s) => s,
        Err(e) => {
            send(ended(Some(e.to_string())));
            return;
        }
    };
    send(NetMsg::ShellOpened {
        session_id: session_id.clone(),
    });
    loop {
        tokio::select! {
            msg = shell.next() => match msg {
                Ok(Some(ShellMsg::Output(bytes))) => send(NetMsg::ShellOutput {
                    session_id: session_id.clone(),
                    bytes,
                }),
                Ok(Some(ShellMsg::Title(title))) => send(NetMsg::ShellTitle {
                    session_id: session_id.clone(),
                    title,
                }),
                Ok(None) => {
                    send(ended(None));
                    return;
                }
                Err(e) => {
                    send(ended(Some(e.to_string())));
                    return;
                }
            },
            req = out.recv() => {
                let res = match req {
                    Some(ShellOut::Input(b)) => shell.input(&b).await,
                    Some(ShellOut::Resize(c, r)) => shell.resize(c, r).await,
                    None => {
                        shell.close().await;
                        return;
                    }
                };
                if let Err(e) = res {
                    send(ended(Some(e.to_string())));
                    return;
                }
            }
        }
    }
}

/// 0.5 s doubling to 8 s.
fn backoff(attempt: u32) -> Duration {
    let ms = 500u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(4));
    Duration::from_millis(ms.min(8000))
}
