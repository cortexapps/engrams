//! Shared runtime for in-guest harness adapters.
//!
//! The agent process and its native protocol remain adapter-owned. This crate
//! owns the failure-prone Engram-facing half: dial/attach, reconnect across
//! snapshot restores and host rolls, and at-least-once event delivery.

pub mod browser_activity;
pub mod browser_view;
mod first_output;
pub mod mode_stamp;
pub mod parked;
pub mod plan;
pub mod questions;
pub mod state;
pub mod turn_context;

use std::collections::{HashSet, VecDeque};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engram_harness_proto::attach_token::AttachToken;
use engram_harness_proto::{
    read_msg, write_msg, AttachReject, HarnessAttach, HarnessAttachAck, HarnessCommand,
    HarnessEvent, HarnessFrame,
};
use engram_ids::SessionId;
use std::future::Future;
use std::pin::Pin;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, Notify};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
pub type BoxedReader = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxedWriter = Box<dyn AsyncWrite + Unpin + Send>;
pub const UNACKED_MAX: usize = 1024;

struct Outbox {
    next_seq: u64,
    unacked: VecDeque<(u64, u64, HarnessEvent)>,
    /// The binding epoch the last sequenced event carried. A connection at
    /// a higher epoch is a new harness generation at the coordinator.
    last_epoch: Option<u64>,
    /// Runs this process started and has not finished, in start order,
    /// tracked from the events it sequences. On a new generation they are
    /// announced as continued BEFORE any buffered event, so the
    /// coordinator's settlement never interrupts a run that is still here.
    open_runs: Vec<String>,
}

struct EventOutbox {
    state: Mutex<Outbox>,
    acked: Notify,
    /// Drawn once per process (see `HarnessFrame::SeqEvent::incarnation`).
    incarnation: u64,
}

impl EventOutbox {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        Self {
            state: Mutex::new(Outbox {
                next_seq: 1,
                unacked: VecDeque::new(),
                last_epoch: None,
                open_runs: Vec::new(),
            }),
            acked: Notify::new(),
            incarnation: nanos ^ (u64::from(std::process::id()) << 32),
        }
    }

    /// Sequence one event under `binding_epoch`: it enters the unacked
    /// window and the open-run set follows it.
    fn sequence(&self, binding_epoch: u64, event: HarnessEvent) -> u64 {
        let mut state = self.state.lock().expect("outbox lock");
        match &event {
            HarnessEvent::RunStarted { run_id, .. } if !state.open_runs.contains(run_id) => {
                state.open_runs.push(run_id.clone());
            }
            HarnessEvent::RunCompleted { run_id, .. }
            | HarnessEvent::RunInterrupted { run_id, .. } => {
                state.open_runs.retain(|r| r != run_id);
            }
            _ => {}
        }
        let seq = state.next_seq;
        state.next_seq = seq
            .checked_add(1)
            .expect("harness event sequence exhausted");
        state.last_epoch = Some(binding_epoch);
        state.unacked.push_back((binding_epoch, seq, event));
        seq
    }

    /// The runs to announce as continued when a connection opens at
    /// `binding_epoch`: every open run, iff the epoch advanced since the
    /// last sequenced event. A reconnect at the same epoch announces nothing
    /// (no settlement happens there); a fresh process has no open runs.
    fn continued_runs(&self, binding_epoch: u64) -> Vec<String> {
        let state = self.state.lock().expect("outbox lock");
        match state.last_epoch {
            Some(last) if binding_epoch > last => state.open_runs.clone(),
            _ => Vec::new(),
        }
    }

    fn len(&self) -> usize {
        self.state.lock().expect("outbox lock").unacked.len()
    }

    fn acknowledge(&self, seq: u64) {
        let mut state = self.state.lock().expect("outbox lock");
        if seq >= state.next_seq {
            tracing::warn!(
                seq,
                next_seq = state.next_seq,
                "ignoring ack for an unassigned sequence"
            );
            return;
        }
        while state
            .unacked
            .front()
            .is_some_and(|(_, pending, _)| *pending <= seq)
        {
            state.unacked.pop_front();
        }
        self.acked.notify_one();
    }
}

#[derive(Clone, Debug)]
pub struct ConnectionConfig {
    pub connect: Option<String>,
    pub port: Option<u32>,
    pub session_id: SessionId,
    pub harness_version: String,
}

impl ConnectionConfig {
    fn attach(&self, token: AttachToken) -> HarnessAttach {
        HarnessAttach {
            session_id: self.session_id,
            sandbox_id: token.sandbox_id,
            binding_epoch: token.binding_epoch,
            harness_version: self.harness_version.clone(),
        }
    }
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
    event_rx: mpsc::Receiver<HarnessEvent>,
    reattach: Arc<Notify>,
) -> ExitCode {
    let dial_cfg = cfg.clone();
    serve_with(
        cfg,
        move || {
            let cfg = dial_cfg.clone();
            Box::pin(async move { dial(&cfg).await })
        },
        engine,
        command_tx,
        event_rx,
        reattach,
    )
    .await
}

/// Use an injected transport with the production token loader.
pub async fn serve_with<D>(
    cfg: ConnectionConfig,
    dialer: D,
    engine: tokio::task::JoinHandle<ExitCode>,
    command_tx: mpsc::Sender<HarnessCommand>,
    event_rx: mpsc::Receiver<HarnessEvent>,
    reattach: Arc<Notify>,
) -> ExitCode
where
    D: FnMut() -> BoxFuture<Option<(BoxedReader, BoxedWriter)>>,
{
    serve_loop(
        cfg,
        dialer,
        AttachToken::load,
        engine,
        command_tx,
        event_rx,
        reattach,
    )
    .await
}

/// In-process fixtures that need the token to CHANGE between dials (a new
/// generation after a snapshot) supply their own loader.
pub async fn serve_with_loader<D, L>(
    cfg: ConnectionConfig,
    dialer: D,
    load_token: L,
    engine: tokio::task::JoinHandle<ExitCode>,
    command_tx: mpsc::Sender<HarnessCommand>,
    event_rx: mpsc::Receiver<HarnessEvent>,
    reattach: Arc<Notify>,
) -> ExitCode
where
    D: FnMut() -> BoxFuture<Option<(BoxedReader, BoxedWriter)>>,
    L: FnMut() -> std::io::Result<AttachToken>,
{
    serve_loop(
        cfg, dialer, load_token, engine, command_tx, event_rx, reattach,
    )
    .await
}

/// In-process fixtures supply a token without changing process-global env.
pub async fn serve_with_token<D>(
    cfg: ConnectionConfig,
    dialer: D,
    token: AttachToken,
    engine: tokio::task::JoinHandle<ExitCode>,
    command_tx: mpsc::Sender<HarnessCommand>,
    event_rx: mpsc::Receiver<HarnessEvent>,
    reattach: Arc<Notify>,
) -> ExitCode
where
    D: FnMut() -> BoxFuture<Option<(BoxedReader, BoxedWriter)>>,
{
    serve_loop(
        cfg,
        dialer,
        move || Ok(token),
        engine,
        command_tx,
        event_rx,
        reattach,
    )
    .await
}

pub const SUPERSEDED_TOKEN_GRACE: Duration = Duration::from_secs(15);

async fn serve_loop<D, L>(
    cfg: ConnectionConfig,
    mut dialer: D,
    mut load_token: L,
    engine: tokio::task::JoinHandle<ExitCode>,
    command_tx: mpsc::Sender<HarnessCommand>,
    mut event_rx: mpsc::Receiver<HarnessEvent>,
    reattach: Arc<Notify>,
) -> ExitCode
where
    D: FnMut() -> BoxFuture<Option<(BoxedReader, BoxedWriter)>>,
    L: FnMut() -> std::io::Result<AttachToken>,
{
    let mut token = match load_token() {
        Ok(token) => token,
        Err(e) => {
            tracing::error!(error = %e, "attach token unavailable");
            drop(command_tx);
            event_rx.close();
            while event_rx.recv().await.is_some() {}
            let _ = engine.await;
            return ExitCode::from(2);
        }
    };
    let outbox = EventOutbox::new();
    // Outlives each connection: a reconnect mid-turn must not lose the
    // turn's stopwatch (that reconnect is often the interesting case).
    let mut first_output = first_output::FirstOutput::default();
    let mut reconnect_nudge =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
            .expect("install SIGUSR1 handler");
    let mut failures = 0u32;

    loop {
        if let Ok(current) = load_token() {
            token = current;
        } else {
            tracing::warn!("attach token reload failed; retaining last token");
        }
        if engine.is_finished() && event_rx.is_empty() && outbox.len() == 0 {
            break;
        }
        // ADR 0108 A1: the dial runs INSIDE the SIGUSR1 select. A dial that
        // hangs (vsock connect has no protocol-level timeout) must stay
        // nudge-able — before this, a hung dial was the one state the
        // reattach fallback could not break.
        let outcome = tokio::select! {
            o = async {
                match tokio::time::timeout(DIAL_TIMEOUT, dialer()).await {
                    Ok(Some(stream)) => {
                        run_one_connection(
                            stream,
                            &cfg.attach(token),
                            &command_tx,
                            &mut event_rx,
                            &outbox,
                            &reattach,
                            &mut first_output,
                        )
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
            ConnOutcome::EngineDone => break,
            ConnOutcome::Superseded => {
                let deadline = tokio::time::Instant::now() + SUPERSEDED_TOKEN_GRACE;
                loop {
                    if let Ok(current) = load_token() {
                        if current.binding_epoch > token.binding_epoch {
                            token = current;
                            break;
                        }
                    }
                    if tokio::time::Instant::now() >= deadline {
                        tracing::warn!(
                            epoch = token.binding_epoch,
                            "attach fence remained after token grace"
                        );
                        drop(command_tx);
                        // Drain the close-out events even though this generation cannot send.
                        let unacked = outbox.len();
                        let mut undelivered = 0;
                        while event_rx.recv().await.is_some() {
                            undelivered += 1;
                        }
                        tracing::warn!(unacked, undelivered, "fenced harness exit");
                        return engine.await.unwrap_or(ExitCode::FAILURE);
                    }
                    tokio::select! {
                        _ = reconnect_nudge.recv() => {},
                        _ = tokio::time::sleep_until(deadline) => {},
                        _ = tokio::time::sleep(Duration::from_millis(500)) => {},
                    }
                }
                failures = 0;
            }
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
    let unacked = outbox.len();
    let mut undelivered = 0;
    while event_rx.recv().await.is_some() {
        undelivered += 1;
    }
    tracing::info!(unacked, undelivered, "harness transport exit");
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
    attach: &HarnessAttach,
    command_tx: &mpsc::Sender<HarnessCommand>,
    event_rx: &mut mpsc::Receiver<HarnessEvent>,
    outbox: &EventOutbox,
    reattach: &Notify,
    first_output: &mut first_output::FirstOutput,
) -> ConnOutcome {
    let handshake = async {
        if write_msg(&mut writer, attach).await.is_err() {
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
        reason = forward_commands(&mut reader, command_tx, outbox) => ConnOutcome::Dropped(reason),
        result = pump_events(&mut writer, event_rx, outbox, first_output, attach.binding_epoch) => result,
    }
}

async fn forward_commands<R: AsyncRead + Unpin>(
    reader: &mut R,
    tx: &mpsc::Sender<HarnessCommand>,
    outbox: &EventOutbox,
) -> &'static str {
    loop {
        match read_msg::<_, HarnessFrame>(reader).await {
            Ok(HarnessFrame::Command(command)) => {
                // Never await the engine's command channel here: this reader
                // is also the only consumer of EventAck, and an engine that
                // is blocked on event backpressure cannot drain commands. A
                // full channel drops the connection instead; the host
                // redelivers durable prompts on the redial.
                match tx.try_send(command) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Closed(_)) => return "engine gone",
                    Err(mpsc::error::TrySendError::Full(_)) => return "command overflow",
                }
            }
            Ok(HarnessFrame::EventAck { seq }) => outbox.acknowledge(seq),
            Ok(HarnessFrame::Event(_) | HarnessFrame::SeqEvent { .. }) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return "eof",
            Err(_) => return "read error",
        }
    }
}

async fn pump_events<W: AsyncWrite + Unpin>(
    writer: &mut W,
    rx: &mut mpsc::Receiver<HarnessEvent>,
    outbox: &EventOutbox,
    first_output: &mut first_output::FirstOutput,
    binding_epoch: u64,
) -> ConnOutcome {
    // Replays keep the epoch they were sequenced under.
    let replay = outbox.state.lock().expect("outbox lock").unacked.clone();
    for (binding_epoch, seq, event) in replay {
        if write_msg(
            writer,
            &HarnessFrame::SeqEvent {
                binding_epoch,
                seq,
                event,
                incarnation: outbox.incarnation,
            },
        )
        .await
        .is_err()
        {
            return ConnOutcome::Dropped("write error");
        }
    }
    // A new generation: the runs still open in this process are announced
    // before any event the engine buffered across the cut, so the first
    // event the coordinator sees at this epoch names them.
    for run_id in outbox.continued_runs(binding_epoch) {
        let event = HarnessEvent::RunContinued { run_id };
        let seq = outbox.sequence(binding_epoch, event.clone());
        if write_msg(
            writer,
            &HarnessFrame::SeqEvent {
                binding_epoch,
                seq,
                event,
                incarnation: outbox.incarnation,
            },
        )
        .await
        .is_err()
        {
            return ConnOutcome::Dropped("write error");
        }
    }
    loop {
        while outbox.len() >= UNACKED_MAX {
            outbox.acked.notified().await;
        }
        let Some(event) = rx.recv().await else {
            while outbox.len() != 0 {
                outbox.acked.notified().await;
            }
            return ConnOutcome::EngineDone;
        };
        first_output.observe(&event);
        // Park before the first await. Cancellation cannot lose this event.
        let seq = outbox.sequence(binding_epoch, event.clone());
        if write_msg(
            writer,
            &HarnessFrame::SeqEvent {
                binding_epoch,
                seq,
                event,
                incarnation: outbox.incarnation,
            },
        )
        .await
        .is_err()
        {
            return ConnOutcome::Dropped("write error");
        }
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
        let outbox = EventOutbox::new();
        let outcome = run_one_connection(
            (Box::new(reader), Box::new(writer)),
            &cfg.attach(AttachToken {
                sandbox_id: engram_ids::SandboxId::new(),
                binding_epoch: 1,
            }),
            &command_tx,
            &mut event_rx,
            &outbox,
            &Notify::new(),
            &mut first_output::FirstOutput::default(),
        )
        .await;
        host_task.abort();
        assert!(
            matches!(outcome, ConnOutcome::Dropped("attach handshake timeout")),
            "a silent host must produce a bounded Dropped outcome",
        );
    }

    #[tokio::test]
    async fn replayed_events_keep_their_sequencing_epoch() {
        let outbox = EventOutbox::new();
        let (tx, mut rx) = mpsc::channel(8);
        let mut first_output = first_output::FirstOutput::default();
        tx.send(HarnessEvent::Busy).await.unwrap();
        for binding_epoch in [4, 5] {
            let (client, mut reader) = tokio::io::duplex(4096);
            let (client_reader, client_writer) = tokio::io::split(client);
            let token = AttachToken {
                sandbox_id: engram_ids::SandboxId::new(),
                binding_epoch,
            };
            let attach = HarnessAttach {
                session_id: SessionId::new(),
                sandbox_id: token.sandbox_id,
                binding_epoch: token.binding_epoch,
                harness_version: "test".into(),
            };
            let (command_tx, _command_rx) = mpsc::channel(1);
            let reattach = Notify::new();
            let pump = run_one_connection(
                (Box::new(client_reader), Box::new(client_writer)),
                &attach,
                &command_tx,
                &mut rx,
                &outbox,
                &reattach,
                &mut first_output,
            );
            tokio::pin!(pump);
            tokio::select! {
                _ = &mut pump => panic!("pump must stay connected"),
                () = async {
                    let received: HarnessAttach = read_msg(&mut reader).await.unwrap();
                    assert_eq!(received.binding_epoch, binding_epoch);
                    write_msg(&mut reader, &HarnessAttachAck { ok: true, reject: None, message: None }).await.unwrap();
                    assert!(matches!(
                        read_msg::<_, HarnessFrame>(&mut reader).await.unwrap(),
                        HarnessFrame::SeqEvent { binding_epoch: 4, seq: 1, event: HarnessEvent::Busy, .. }
                    ));
                    if binding_epoch == 5 {
                        tx.send(HarnessEvent::Idle).await.unwrap();
                        assert!(matches!(
                            read_msg::<_, HarnessFrame>(&mut reader).await.unwrap(),
                            HarnessFrame::SeqEvent { binding_epoch: 5, seq: 2, event: HarnessEvent::Idle, .. }
                        ));
                    }
                } => {}
            }
        }
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
