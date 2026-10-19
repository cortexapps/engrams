//! The network side: one task per attached session (status, the history
//! tail, then the SSE loop with reconnect) and fire-and-forget RPCs for
//! actions. Everything reports back through one channel as `NetMsg`.

use std::time::Duration;

use engram_app_client::proto::*;
use engram_app_client::sse::EventStream;
use engram_app_client::Clients;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

use crate::app::{Msg, NetMsg, PAGE};
use crate::event::Envelope;
use crate::sessions::rows_from_tasks;

pub struct Net {
    clients: Clients,
    tx: UnboundedSender<Msg>,
    attach: Option<JoinHandle<()>>,
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
        }
    }

    pub fn load_sessions(&self) {
        let c = self.clients.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = c
                .list_tasks(ListTasksRequest {
                    page_size: 200,
                    ..Default::default()
                })
                .await
                .map(|r| rows_from_tasks(&r.tasks, now_unix()))
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::Net(NetMsg::Sessions(res)));
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
        if let Some(h) = self.attach.take() {
            h.abort();
        }
    }

    pub fn load_older(&self, session_id: String, before_idx: i64) {
        let c = self.clients.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match list_page(&c, &session_id, before_idx).await {
                Ok(events) => {
                    let _ = tx.send(Msg::Net(NetMsg::History {
                        session_id,
                        events,
                        older: true,
                    }));
                }
                Err(error) => {
                    let _ = tx.send(Msg::Net(NetMsg::Failed { session_id, error }));
                }
            }
        });
    }

    pub fn send_prompt(&self, session_id: String, prompt_id: String, text: String) {
        let c = self.clients.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = c
                .send_prompt(SendPromptRequest {
                    session_id: session_id.clone(),
                    text,
                    prompt_id: prompt_id.clone(),
                    harness_mode: None,
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
    .map_err(|e| e.to_string())
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
            error: e.to_string(),
        }),
    }
    let mut since: Option<i64> = None;
    match list_page(&c, &session_id, i64::MAX).await {
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
                    error: e.to_string(),
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

/// 0.5 s doubling to 8 s.
fn backoff(attempt: u32) -> Duration {
    let ms = 500u64.saturating_mul(1u64 << attempt.saturating_sub(1).min(4));
    Duration::from_millis(ms.min(8000))
}
