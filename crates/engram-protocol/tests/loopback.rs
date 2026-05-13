//! End-to-end test of the bincode-over-WS Frame protocol via a pair of
//! in-memory mpsc channels. Proves a coordinator-side
//! `RemoteSandboxBackend` round-trips create/exec/destroy through a
//! host-side `HostSession` driving a real `ProcessBackend`.
//!
//! No actual WebSocket handshake — the frame protocol only needs
//! message-oriented byte channels. Ping/Pong/Close don't appear here
//! because nothing on either side emits them; if we add real liveness
//! probes those land in the WS layer above this protocol.

use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::{HostClient, SandboxBackend};
use engram_core::types::sandbox::{
    CpuLimit, DiskLimit, ExecEvent, ExecRequest, MemoryLimit, SandboxSpec,
};
use engram_host_agent::harness::{event_sink_to, HarnessHub};
use engram_host_agent::LocalHostClient;
use engram_protocol::client::{ConnectedHost, RemoteHostClient};
use engram_protocol::server::HostSession;
use engram_sandbox_process::ProcessBackend;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::{Error as TungError, Message as TungMessage};

fn live_spec() -> SandboxSpec {
    SandboxSpec {
        image: "loopback-test".into(),
        rootfs_source: None,
        image_uri: None,
        harness_pack_uri: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 64 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: Default::default(),
        workdir: None,
        harness_substrate: None,
        network: Default::default(),
        canonical_memory_manifest: None,
    }
}

/// Wire one ConnectedHost (coord side) to one HostSession (host side)
/// via a pair of mpsc channels. Wraps the supplied VMM backend in a
/// `LocalHostClient` so the server-side `HostSession` sees the full
/// `HostClient` surface (sandbox + harness ops). Returns the coord-
/// side `RemoteHostClient` and a JoinHandle for the host-side serve
/// loop.
async fn pair(backend: Arc<dyn SandboxBackend>) -> (RemoteHostClient, tokio::task::JoinHandle<()>) {
    // A: coord -> host    B: host -> coord
    let (coord_tx_a, host_rx_a) = futures::channel::mpsc::unbounded::<TungMessage>();
    let (host_tx_b, coord_rx_b) = futures::channel::mpsc::unbounded::<TungMessage>();

    let coord_sink =
        coord_tx_a.sink_map_err(|e| TungError::Io(std::io::Error::other(e.to_string())));
    let coord_stream = coord_rx_b.map(Ok::<TungMessage, TungError>);

    let host_sink = host_tx_b.sink_map_err(|e| TungError::Io(std::io::Error::other(e.to_string())));
    let host_stream = host_rx_a.map(Ok::<TungMessage, TungError>);

    let (connected, _notify_rx, _demux) =
        ConnectedHost::spawn(Box::pin(coord_sink), Box::pin(coord_stream));
    let session = HostSession::new(Box::pin(host_sink));

    let hub = Arc::new(HarnessHub::new(event_sink_to(|_, _, _| async {})));
    let local: Arc<dyn HostClient> = Arc::new(LocalHostClient::new(backend, hub));
    let serve_handle = tokio::spawn(async move {
        session
            .serve_with_reader(local, None, None, Box::pin(host_stream))
            .await;
    });

    (RemoteHostClient::new(connected), serve_handle)
}

#[tokio::test]
async fn create_and_destroy_round_trip() {
    let dir = TempDir::new().unwrap();
    let local: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(dir.path()));
    let (remote, _serve) = pair(local).await;

    let id = remote
        .create(live_spec())
        .await
        .expect("create round-trips");
    let listed = remote.list().await.expect("list round-trips");
    assert!(
        listed.contains(&id),
        "list must include the created sandbox"
    );
    remote.destroy(id).await.expect("destroy round-trips");
}

#[tokio::test]
async fn exec_stream_round_trips_stdout_then_exit() {
    let dir = TempDir::new().unwrap();
    let local: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(dir.path()));
    let (remote, _serve) = pair(local).await;

    let id = remote.create(live_spec()).await.unwrap();
    let mut stream = remote
        .exec_stream(
            id,
            ExecRequest {
                command: vec!["sh".into(), "-c".into(), "printf hi".into()],
                stdin: None,
                env: Default::default(),
                workdir: None,
                timeout: Some(Duration::from_secs(5)),
            },
        )
        .await
        .expect("exec_stream round-trips");

    let mut stdout = Vec::new();
    let mut got_exit = None;
    while let Some(ev) = stream.events.next().await {
        match ev {
            ExecEvent::Stdout(b) => stdout.extend_from_slice(&b),
            ExecEvent::Stderr(_) => {}
            ExecEvent::Exit(code) => {
                got_exit = Some(code);
                break;
            }
        }
    }
    assert_eq!(stdout, b"hi", "stdout must round-trip byte-for-byte");
    assert_eq!(got_exit, Some(Some(0)));
}

#[tokio::test]
async fn exec_stream_propagates_nonzero_exit_via_wire() {
    let dir = TempDir::new().unwrap();
    let local: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(dir.path()));
    let (remote, _serve) = pair(local).await;

    let id = remote.create(live_spec()).await.unwrap();
    let mut stream = remote
        .exec_stream(
            id,
            ExecRequest {
                command: vec!["sh".into(), "-c".into(), "exit 7".into()],
                stdin: None,
                env: Default::default(),
                workdir: None,
                timeout: Some(Duration::from_secs(5)),
            },
        )
        .await
        .unwrap();

    let mut got_exit = None;
    while let Some(ev) = stream.events.next().await {
        if let ExecEvent::Exit(code) = ev {
            got_exit = Some(code);
            break;
        }
    }
    assert_eq!(got_exit, Some(Some(7)));
}

#[tokio::test]
async fn exec_stderr_round_trips_separately_from_stdout() {
    let dir = TempDir::new().unwrap();
    let local: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(dir.path()));
    let (remote, _serve) = pair(local).await;

    let id = remote.create(live_spec()).await.unwrap();
    let mut stream = remote
        .exec_stream(
            id,
            ExecRequest {
                command: vec![
                    "sh".into(),
                    "-c".into(),
                    "printf out; printf 'err' 1>&2".into(),
                ],
                stdin: None,
                env: Default::default(),
                workdir: None,
                timeout: Some(Duration::from_secs(5)),
            },
        )
        .await
        .unwrap();

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while let Some(ev) = stream.events.next().await {
        match ev {
            ExecEvent::Stdout(b) => stdout.extend_from_slice(&b),
            ExecEvent::Stderr(b) => stderr.extend_from_slice(&b),
            ExecEvent::Exit(_) => break,
        }
    }
    assert_eq!(stdout, b"out");
    assert_eq!(stderr, b"err");
}

#[tokio::test]
async fn start_agent_round_trips_argv_and_env_through_the_wire() {
    // Coord sends StartAgent over the WS, host's HostSession decodes
    // and forwards to ProcessBackend::start_agent, which spawns the
    // argv. Inspect the on-disk side effect (`marker` file) to confirm
    // both argv elements and env entries crossed the wire intact.
    let dir = TempDir::new().unwrap();
    let local: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(dir.path()));
    let (remote, _serve) = pair(local).await;

    let id = remote.create(live_spec()).await.unwrap();

    // The sandbox cwd is `<root>/<sandbox-id>/`. ProcessBackend spawns
    // the agent with that cwd, so a relative shell redirect lands in
    // a known location.
    let agent = engram_core::types::sandbox::AgentSpec {
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '%s' \"$MARKER_VALUE\" > marker; sleep 5".into(),
        ],
        env: [("MARKER_VALUE".to_string(), "wire-round-trip".to_string())]
            .into_iter()
            .collect(),
    };
    remote
        .start_agent(id, agent)
        .await
        .expect("start_agent round-trips");

    // The sh redirect happens before sleep; poll briefly so we don't
    // race the fork+exec.
    let marker_path = dir.path().join(id.to_string()).join("marker");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !marker_path.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let body = std::fs::read_to_string(&marker_path).expect("marker file written");
    assert_eq!(body, "wire-round-trip");

    let _ = remote.destroy(id).await;
}

#[tokio::test]
async fn start_agent_propagates_invalid_spec_error_through_the_wire() {
    // Empty argv → ProcessBackend returns InvalidSpec. Confirms the
    // wire surfaces backend errors as SandboxError rather than
    // swallowing them into a generic transport failure.
    let dir = TempDir::new().unwrap();
    let local: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(dir.path()));
    let (remote, _serve) = pair(local).await;

    let id = remote.create(live_spec()).await.unwrap();
    let err = remote
        .start_agent(
            id,
            engram_core::types::sandbox::AgentSpec {
                argv: Vec::new(),
                env: Default::default(),
            },
        )
        .await
        .expect_err("empty argv must propagate as a backend error");
    assert!(
        matches!(err, engram_core::SandboxError::InvalidSpec(_)),
        "expected InvalidSpec, got {err:?}",
    );
    let _ = remote.destroy(id).await;
}

#[tokio::test]
async fn destroy_unknown_sandbox_id_is_idempotent_through_the_wire() {
    // ProcessBackend's destroy is intentionally lenient (it's the
    // dev backend; the destroyed-twice path has to work because pool
    // teardown can race). The wire mustn't silently turn that into a
    // failure — confirms RemoteSandboxBackend faithfully forwards a
    // backend's success status, not just its error status.
    let dir = TempDir::new().unwrap();
    let local: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(dir.path()));
    let (remote, _serve) = pair(local).await;

    let bogus = engram_core::SandboxId::new();
    remote
        .destroy(bogus)
        .await
        .expect("ProcessBackend's destroy is idempotent on unknown ids; wire must round-trip Ok");
}

#[tokio::test]
async fn harness_event_forwards_via_wire_to_coord_notify_channel() {
    // Phase 2 surface: the host-agent's local `HarnessHub` ships
    // every adapter event over the WS as `NotifyKind::HarnessEvent`,
    // and the coord's read loop replays it through its in-proc
    // hub's EventSink. Here we wire just the host side: build a
    // `HarnessHub` with an `EventSink` closure that drops the event
    // into a `NotifyKind::HarnessEvent` on the host's session, then
    // drive `hub.emit_external(...)` and confirm the coord-side
    // `notify_rx` channel surfaces the same payload.
    use engram_harness_proto::HarnessEvent;
    use engram_protocol::wire::NotifyKind;

    // A: coord -> host    B: host -> coord
    let (coord_tx_a, host_rx_a) = futures::channel::mpsc::unbounded::<TungMessage>();
    let (host_tx_b, coord_rx_b) = futures::channel::mpsc::unbounded::<TungMessage>();

    let coord_sink =
        coord_tx_a.sink_map_err(|e| TungError::Io(std::io::Error::other(e.to_string())));
    let coord_stream = coord_rx_b.map(Ok::<TungMessage, TungError>);
    let host_sink = host_tx_b.sink_map_err(|e| TungError::Io(std::io::Error::other(e.to_string())));
    let _host_stream = host_rx_a.map(Ok::<TungMessage, TungError>);

    let (_connected, mut notify_rx, _demux) =
        ConnectedHost::spawn(Box::pin(coord_sink), Box::pin(coord_stream));
    let session = HostSession::new(Box::pin(host_sink));

    // EventSink closure mirrors what host-agent::lib.rs builds: it
    // captures the live HostSession and ships every event as a
    // NotifyKind::HarnessEvent.
    let session_for_sink = session.clone();
    let event_sink = event_sink_to(move |session_id, sandbox_id, ev| {
        let s = session_for_sink.clone();
        async move {
            let frame = NotifyKind::HarnessEvent {
                session_id,
                sandbox_id,
                event: ev,
                at: chrono::Utc::now(),
            };
            s.notify(frame).await.expect("send harness event over wire");
        }
    });
    let hub = HarnessHub::new(event_sink);
    let session_id = engram_core::SessionId::new();
    let sandbox_id = engram_core::SandboxId::new();
    hub.emit_external(session_id, sandbox_id, HarnessEvent::Idle)
        .await;

    let observed = tokio::time::timeout(Duration::from_secs(2), notify_rx.recv())
        .await
        .expect("notify arrives within timeout")
        .expect("notify channel still open");
    match observed {
        NotifyKind::HarnessEvent {
            session_id: got_sid,
            sandbox_id: got_sandbox,
            event: HarnessEvent::Idle,
            ..
        } => {
            assert_eq!(got_sid, session_id);
            assert_eq!(got_sandbox, sandbox_id);
        }
        other => panic!("expected HarnessEvent::Idle, got {other:?}"),
    }
}
