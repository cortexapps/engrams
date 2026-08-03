//! Shared runtime for in-guest harness adapters.
//!
//! The agent process and its native protocol remain adapter-owned. This crate
//! owns the failure-prone Engram-facing half: dial/attach, reconnect across
//! snapshot restores and host rolls, and at-least-once event delivery.

pub mod browser_activity;
pub mod browser_view;
pub mod mode_stamp;
pub mod parked;
pub mod plan;
pub mod questions;
pub mod state;

use std::collections::{HashSet, VecDeque};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engram_core::{SandboxId, SessionId};
use engram_harness_proto::{
    read_msg, write_msg, AttachReject, HarnessAttach, HarnessAttachAck, HarnessCommand,
    HarnessEvent, HarnessFrame,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, Notify};

pub type BoxedReader = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxedWriter = Box<dyn AsyncWrite + Unpin + Send>;
type HeldEvent = Arc<Mutex<Option<HarnessEvent>>>;

#[derive(Clone, Debug)]
pub struct ConnectionConfig {
    pub connect: Option<String>,
    pub port: Option<u32>,
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    pub binding_epoch: u64,
    pub harness_version: String,
}

pub struct Channels {
    pub command_tx: mpsc::Sender<HarnessCommand>,
    pub command_rx: mpsc::Receiver<HarnessCommand>,
    pub event_tx: mpsc::Sender<HarnessEvent>,
    pub event_rx: mpsc::Receiver<HarnessEvent>,
    pub reattach: Arc<Notify>,
}

impl Channels {
    pub fn new() -> Self {
        let (command_tx, command_rx) = mpsc::channel(16);
        let (event_tx, event_rx) = mpsc::channel(1024);
        Self {
            command_tx,
            command_rx,
            event_tx,
            event_rx,
            reattach: Arc::new(Notify::new()),
        }
    }
}

impl Default for Channels {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn emit(tx: &mpsc::Sender<HarnessEvent>, event: HarnessEvent) {
    // Domain enrichment precedes the generic start so live clients never
    // briefly render a raw Shell/Bash card before replacing it.
    if let Some(activity) = browser_activity::from_tool_call(&event) {
        if tx.send(activity).await.is_err() {
            tracing::debug!("event channel closed; dropping event");
            return;
        }
    }
    if tx.send(event).await.is_err() {
        tracing::debug!("event channel closed; dropping event");
    }
}

/// Continuously drain a child stderr pipe while retaining only its bounded
/// tail for an eventual crash diagnostic. Draining is mandatory: leaving a
/// piped stderr unread can deadlock a verbose agent process.
pub fn spawn_stderr_tail<R>(
    stderr: R,
    max_lines: usize,
    echo: bool,
) -> tokio::task::JoinHandle<Vec<String>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut tail = VecDeque::with_capacity(max_lines.saturating_add(1));
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if echo {
                eprintln!("{line}");
            }
            if tail.len() >= max_lines {
                tail.pop_front();
            }
            tail.push_back(line);
        }
        tail.into_iter().collect()
    })
}

/// Run the host-facing connection loop around an independently-owned engine.
pub async fn serve(
    cfg: ConnectionConfig,
    engine: tokio::task::JoinHandle<ExitCode>,
    command_tx: mpsc::Sender<HarnessCommand>,
    mut event_rx: mpsc::Receiver<HarnessEvent>,
    reattach: Arc<Notify>,
) -> ExitCode {
    let held: HeldEvent = Arc::new(Mutex::new(None));
    let mut reconnect_nudge =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
            .expect("install SIGUSR1 handler");
    let mut failures = 0u32;

    loop {
        if engine.is_finished() {
            break;
        }
        // ADR 0108 A1: the dial runs INSIDE the SIGUSR1 select. A dial that
        // hangs (vsock connect has no protocol-level timeout) must stay
        // nudge-able — before this, a hung dial was the one state the
        // reattach fallback could not break.
        let outcome = tokio::select! {
            o = async {
                match tokio::time::timeout(DIAL_TIMEOUT, dial(&cfg)).await {
                    Ok(Some(stream)) => {
                        run_one_connection(stream, &cfg, &command_tx, &mut event_rx, &held, &reattach)
                            .await
                    }
                    Ok(None) => ConnOutcome::DialFailed,
                    Err(_) => {
                        tracing::warn!(timeout_secs = DIAL_TIMEOUT.as_secs(), "dial timed out");
                        ConnOutcome::DialFailed
                    }
                }
            } => o,
            _ = reconnect_nudge.recv() => ConnOutcome::Dropped("SIGUSR1 reconnect nudge"),
        };
        match outcome {
            ConnOutcome::EngineDone | ConnOutcome::Superseded => break,
            ConnOutcome::DialFailed => {
                failures = failures.saturating_add(1);
                let backoff = std::cmp::min(10, 1u64 << failures.min(4));
                if failures.is_power_of_two() {
                    tracing::warn!(failures, backoff, "host unreachable; retrying indefinitely");
                }
                tokio::time::sleep(Duration::from_secs(backoff)).await;
            }
            ConnOutcome::Dropped(reason) => {
                failures = 0;
                tracing::warn!(reason, "harness connection dropped; reconnecting");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            ConnOutcome::Rejected(reason) => {
                failures = failures.saturating_add(1);
                let backoff = std::cmp::min(10, 1u64 << failures.min(4));
                tracing::warn!(%reason, backoff, "harness attach rejected; retrying");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
            }
        }
    }
    drop(command_tx);
    engine.await.unwrap_or_else(|e| {
        tracing::error!(error = %e, "harness engine task panicked");
        ExitCode::from(1)
    })
}

async fn dial(cfg: &ConnectionConfig) -> Option<(BoxedReader, BoxedWriter)> {
    match (cfg.connect.as_deref(), cfg.port) {
        (Some(addr), None) => match tokio::net::TcpStream::connect(addr).await {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                let (r, w) = tokio::io::split(stream);
                Some((Box::new(r), Box::new(w)))
            }
            Err(e) => {
                tracing::error!(error = %e, addr, "TCP dial failed");
                None
            }
        },
        (None, Some(port)) => match engram_transport::from_env() {
            Ok(transport) => match transport.dial(port).await {
                Ok(stream) => {
                    let (r, w) = tokio::io::split(stream);
                    Some((Box::new(r), Box::new(w)))
                }
                Err(e) => {
                    tracing::error!(error = %e, port, "transport dial failed");
                    None
                }
            },
            Err(e) => {
                tracing::error!(error = %e, "build transport failed");
                None
            }
        },
        _ => None,
    }
}

/// ADR 0108 A1: bound on the dial. A vsock connect can hang with no error
/// when the muxer is mid-churn; the bound turns that into a logged retry.
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// ADR 0108 A1: bound on the attach write + ack read. The 2026-07-31
/// incident: a post-checkpoint vsock `TRANSPORT_RESET` window swallowed the
/// `HarnessAttach` frame, and both sides parked forever on unbounded reads —
/// a 41-second silent zombie whose only escape was a coordinator SIGUSR1.
/// vsock has no retransmit, so the ONLY correct remedy is to give up and
/// redial. Generous: covers a slow host under load; a swallow self-heals in
/// one window instead of a coordinator retry cycle.
const ATTACH_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

enum ConnOutcome {
    EngineDone,
    Rejected(String),
    Superseded,
    Dropped(&'static str),
    /// The dial itself failed or timed out (distinct from `Rejected`: no
    /// connection ever existed, so the host never saw us).
    DialFailed,
}

async fn run_one_connection(
    (mut reader, mut writer): (BoxedReader, BoxedWriter),
    cfg: &ConnectionConfig,
    command_tx: &mpsc::Sender<HarnessCommand>,
    event_rx: &mut mpsc::Receiver<HarnessEvent>,
    held: &HeldEvent,
    reattach: &Notify,
) -> ConnOutcome {
    let attach = HarnessAttach {
        session_id: cfg.session_id,
        sandbox_id: cfg.sandbox_id,
        binding_epoch: cfg.binding_epoch,
        harness_version: cfg.harness_version.clone(),
    };
    let handshake = async {
        if write_msg(&mut writer, &attach).await.is_err() {
            return Err(ConnOutcome::Rejected("attach write failed".into()));
        }
        match read_msg::<_, HarnessAttachAck>(&mut reader).await {
            Ok(ack) if ack.ok => Ok(()),
            Ok(ack) if ack.reject == Some(AttachReject::Superseded) => Err(ConnOutcome::Superseded),
            Ok(ack) => Err(ConnOutcome::Rejected(ack.message.unwrap_or_default())),
            Err(_) => Err(ConnOutcome::Rejected("attach ack read failed".into())),
        }
    };
    match tokio::time::timeout(ATTACH_HANDSHAKE_TIMEOUT, handshake).await {
        Ok(Ok(())) => {}
        Ok(Err(outcome)) => return outcome,
        Err(_) => return ConnOutcome::Dropped("attach handshake timeout"),
    }
    reattach.notify_one();
    tokio::select! {
        reason = forward_commands(&mut reader, command_tx) => ConnOutcome::Dropped(reason),
        result = pump_events(&mut writer, event_rx, held) => result,
    }
}

async fn forward_commands<R: AsyncRead + Unpin>(
    reader: &mut R,
    tx: &mpsc::Sender<HarnessCommand>,
) -> &'static str {
    loop {
        match read_msg::<_, HarnessFrame>(reader).await {
            Ok(HarnessFrame::Command(command)) => {
                if tx.send(command).await.is_err() {
                    return "engine gone";
                }
            }
            Ok(HarnessFrame::Event(_)) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return "eof",
            Err(_) => return "read error",
        }
    }
}

async fn pump_events<W: AsyncWrite + Unpin>(
    writer: &mut W,
    rx: &mut mpsc::Receiver<HarnessEvent>,
    held: &HeldEvent,
) -> ConnOutcome {
    loop {
        let parked = held.lock().expect("held event lock poisoned").clone();
        let event = match parked {
            Some(event) => event,
            None => match rx.recv().await {
                Some(event) => {
                    *held.lock().expect("held event lock poisoned") = Some(event.clone());
                    event
                }
                None => return ConnOutcome::EngineDone,
            },
        };
        if write_msg(writer, &HarnessFrame::Event(event))
            .await
            .is_err()
        {
            return ConnOutcome::Dropped("write error");
        }
        *held.lock().expect("held event lock poisoned") = None;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedPrompt {
    pub prompt_id: String,
    pub text: String,
    /// ADR 0107: the mode directive THIS prompt carried, applied when its turn
    /// starts. It rides the prompt rather than a process-global latch because
    /// the queue holds several prompts under type-ahead: latching on arrival
    /// let a later prompt's mode decide an earlier prompt's turn, including
    /// the unsafe direction where a prompt sent during a read-only pass ran
    /// with full write access (PR #927 review). `None` inherits the session's
    /// current mode rather than resetting it.
    pub mode: Option<String>,
}

#[derive(Default)]
pub struct PromptQueue {
    pending: VecDeque<QueuedPrompt>,
    seen: HashSet<String>,
}

impl PromptQueue {
    pub fn accept(&mut self, prompt_id: String, text: String, mode: Option<String>) -> bool {
        if !self.seen.insert(prompt_id.clone()) {
            return false;
        }
        self.pending.push_back(QueuedPrompt {
            prompt_id,
            text,
            mode,
        });
        true
    }

    pub fn pop_front(&mut self) -> Option<QueuedPrompt> {
        self.pending.pop_front()
    }

    pub fn edit(&mut self, prompt_id: &str, text: String) -> bool {
        let Some(prompt) = self.pending.iter_mut().find(|p| p.prompt_id == prompt_id) else {
            return false;
        };
        prompt.text = text;
        true
    }

    pub fn remove(&mut self, prompt_id: &str) -> bool {
        let Some(index) = self.pending.iter().position(|p| p.prompt_id == prompt_id) else {
            return false;
        };
        self.pending.remove(index);
        true
    }
}

pub fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_harness_proto::HarnessAttachAck;

    #[tokio::test]
    async fn emit_places_browser_enrichment_before_generic_tool_start() {
        let (tx, mut rx) = mpsc::channel(2);
        let start = HarnessEvent::ToolCallStarted {
            run_id: "run-1".into(),
            tool_call_id: "tool-1".into(),
            tool_name: "Shell".into(),
            args_summary: Some(
                r#"ENGRAM_BROWSER_INTENT="Clicking Sign in" playwright-cli click e7"#.into(),
            ),
        };
        emit(&tx, start.clone()).await;
        assert!(matches!(
            rx.recv().await,
            Some(HarnessEvent::BrowserActivity { tool_call_id, .. }) if tool_call_id == "tool-1"
        ));
        assert_eq!(rx.recv().await, Some(start));
    }

    #[test]
    fn prompt_queue_deduplicates_and_remains_editable() {
        let mut queue = PromptQueue::default();
        assert!(queue.accept("p1".into(), "first".into(), None));
        assert!(!queue.accept("p1".into(), "replay".into(), None));
        assert!(queue.edit("p1", "edited".into()));
        assert_eq!(queue.pop_front().unwrap().text, "edited");

        assert!(queue.accept("p2".into(), "remove me".into(), None));
        assert!(queue.remove("p2"));
        assert!(queue.pop_front().is_none());
    }

    /// ADR 0107 (PR #927 review): the mode has to ride the PROMPT. Latching a
    /// process-global stamp on arrival meant a later queued prompt's mode
    /// decided an earlier one's turn — and in the unsafe direction, a prompt
    /// sent during a read-only pass ran with full write access because a later
    /// prompt had switched back to build.
    #[test]
    fn a_queued_prompt_keeps_the_mode_it_was_sent_with() {
        let mut queue = PromptQueue::default();
        // A is sent during a plan pass (no directive: inherit plan).
        assert!(queue.accept("a".into(), "audit the code".into(), None));
        // B switches back to build.
        assert!(queue.accept("b".into(), "now fix it".into(), Some("default".into())));

        let a = queue.pop_front().expect("A first");
        assert_eq!(a.prompt_id, "a");
        assert_eq!(a.mode, None, "A carries no directive — it inherits plan");
        let b = queue.pop_front().expect("B second");
        assert_eq!(b.prompt_id, "b");
        assert_eq!(
            b.mode.as_deref(),
            Some("default"),
            "B's switch belongs to B's turn, not A's"
        );
    }

    #[test]
    fn truncation_never_splits_utf8() {
        assert_eq!(truncate_utf8("éclair", 1), "…[truncated]");
        assert_eq!(truncate_utf8("éclair", 2), "é…[truncated]");
    }

    #[tokio::test]
    async fn superseded_attach_is_terminal() {
        let (client, mut host) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(client);
        let cfg = ConnectionConfig {
            connect: Some("unused".into()),
            port: None,
            session_id: SessionId::new(),
            sandbox_id: SandboxId::new(),
            binding_epoch: 7,
            harness_version: "test".into(),
        };
        let host_task = tokio::spawn(async move {
            let _: HarnessAttach = read_msg(&mut host).await.unwrap();
            write_msg(
                &mut host,
                &HarnessAttachAck {
                    ok: false,
                    reject: Some(AttachReject::Superseded),
                    message: None,
                },
            )
            .await
            .unwrap();
        });
        let (command_tx, _) = mpsc::channel(1);
        let (_, mut event_rx) = mpsc::channel(1);
        let held = Arc::new(Mutex::new(None));
        let outcome = run_one_connection(
            (Box::new(reader), Box::new(writer)),
            &cfg,
            &command_tx,
            &mut event_rx,
            &held,
            &Notify::new(),
        )
        .await;
        host_task.await.unwrap();
        assert!(matches!(outcome, ConnOutcome::Superseded));
    }

    /// ADR 0108 A1: an attach whose ack never arrives (the swallowed-
    /// frame vsock window — the 2026-07-31 41-second zombie) must not
    /// park the SDK forever. The handshake bound turns the hang into a
    /// `Dropped` outcome; the serve loop answers with a fast redial —
    /// no coordinator SIGUSR1 required.
    #[tokio::test(start_paused = true)]
    async fn attach_handshake_times_out_instead_of_hanging() {
        let (client, mut host) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(client);
        let cfg = ConnectionConfig {
            connect: Some("unused".into()),
            port: None,
            session_id: SessionId::new(),
            sandbox_id: SandboxId::new(),
            binding_epoch: 1,
            harness_version: "test".into(),
        };
        // The host reads the attach, then goes silent: it never acks
        // and never closes — the exact black-hole shape.
        let host_task = tokio::spawn(async move {
            let _: HarnessAttach = read_msg(&mut host).await.unwrap();
            std::future::pending::<()>().await;
        });
        let (command_tx, _) = mpsc::channel(1);
        let (_, mut event_rx) = mpsc::channel(1);
        let held = Arc::new(Mutex::new(None));
        let outcome = run_one_connection(
            (Box::new(reader), Box::new(writer)),
            &cfg,
            &command_tx,
            &mut event_rx,
            &held,
            &Notify::new(),
        )
        .await;
        host_task.abort();
        assert!(
            matches!(outcome, ConnOutcome::Dropped("attach handshake timeout")),
            "a silent host must produce a bounded Dropped outcome",
        );
    }

    #[test]
    fn channels_apply_bounded_backpressure() {
        let channels = Channels::new();
        for _ in 0..1024 {
            channels.event_tx.try_send(HarnessEvent::Idle).unwrap();
        }
        assert!(channels.event_tx.try_send(HarnessEvent::Idle).is_err());
    }
}
