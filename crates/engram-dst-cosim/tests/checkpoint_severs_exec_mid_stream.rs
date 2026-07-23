//! ADR 0103 regression for prod session `6d403c4b` on 2026-07-23.
//!
//! The review bootstrap emitted `exec_started` at 15:03:46 and one
//! `Cloning into…` stderr chunk. A periodic checkpoint completed at 15:04:09,
//! its vsock `TRANSPORT_RESET` silently forgot the guest connection without
//! host EOF, and the caller then saw 20+ minutes of silence. Before durable
//! exec, the host reader parked in `read_msg` forever and the coordinator
//! never persisted `exec_completed`.
//!
//! This scenario composes the real coordinator `exec_stream_core`, the real
//! Firecracker host protocol driver, and a real agentd journal handler. Only
//! the severed vsock transport is modeled. The test uses real Tokio time
//! rather than `start_paused`: the command is a real subprocess, while
//! paused-time auto-advance can consume the 40×25ms reconnect backoff and
//! agentd's missing-pid grace window before wall-clock process I/O runs. A
//! FIFO orders the checkpoint between the first chunk and command completion;
//! no sleep establishes correctness.

use std::collections::HashMap;
use std::fmt::Debug;
use std::time::Duration;

use engram_coordinator::api::exec::{exec_stream_core, ExecRequest, ExecStreamEvent};
use engram_core::types::session::SessionState;
use engram_dst_cosim::Cosim;
use futures_util::{Stream, StreamExt};
use tokio::io::AsyncWriteExt;

const WEDGE_BOUND: Duration = Duration::from_secs(20);
const EXEC_ID: &str = "exec:cosim:bootstrap-clone";
const FIRST_STDERR: &[u8] = b"Cloning into '/workspace/engrams'...\n";

async fn next_event<S, E>(stream: &mut S, wedge: &str) -> ExecStreamEvent
where
    S: Stream<Item = Result<ExecStreamEvent, E>> + Unpin,
    E: Debug,
{
    let item = tokio::time::timeout(WEDGE_BOUND, stream.next())
        .await
        .unwrap_or_else(|_| panic!("{wedge}: timed out"));
    let result = item.unwrap_or_else(|| panic!("{wedge}: stream ended without terminal Exit"));
    result.unwrap_or_else(|error| panic!("{wedge}: coordinator stream error: {error:?}"))
}

#[tokio::test]
async fn periodic_checkpoint_reattaches_exec_without_gaps_duplicates_or_respawn() {
    let mut sim = tokio::time::timeout(WEDGE_BOUND, Cosim::new(0x0103_C051))
        .await
        .expect("wedge: constructing the co-sim world");
    let session = tokio::time::timeout(WEDGE_BOUND, sim.boot_session())
        .await
        .expect("wedge: boot_session did not reach its terminal step");
    let state = tokio::time::timeout(WEDGE_BOUND, sim.session_state(session))
        .await
        .expect("wedge: reading the booted session state");
    assert_eq!(state, Some(SessionState::Active));
    let sandbox = tokio::time::timeout(WEDGE_BOUND, sim.sandbox_of(session))
        .await
        .expect("wedge: reading the Active session's sandbox")
        .expect("Active session is bound to a sandbox");

    let temp = tempfile::tempdir().expect("scenario tempdir");
    let marker = temp.path().join("spawn-marker");
    let gate = temp.path().join("checkpoint-gate");
    let mkfifo = std::process::Command::new("mkfifo")
        .arg(&gate)
        .status()
        .expect("spawn mkfifo");
    assert!(mkfifo.success(), "mkfifo failed with {mkfifo}");

    let script = concat!(
        "printf \"Cloning into '/workspace/engrams'...\\n\" >&2\n",
        "printf 'spawn\\n' >> \"$1\"\n",
        "printf 'stdout-before\\n'\n",
        "IFS= read -r _ < \"$2\"\n",
        "printf 'stdout-after\\n'\n",
        "printf 'stderr-after\\n' >&2\n",
        "exit 7\n",
    );
    let request = ExecRequest {
        command: None,
        argv: Some(vec![
            "sh".into(),
            "-c".into(),
            script.into(),
            "cosim-bootstrap".into(),
            marker.to_string_lossy().into_owned(),
            gate.to_string_lossy().into_owned(),
        ]),
        env: HashMap::new(),
        workdir: None,
        timeout_secs: None,
        exec_id: Some(EXEC_ID.into()),
        stdout_offset: None,
        stderr_offset: None,
        wake: Some(false),
    };
    let (exec_id, mut stream) = tokio::time::timeout(
        WEDGE_BOUND,
        exec_stream_core(&sim.world.state, session, request),
    )
    .await
    .expect("wedge: exec_stream_core did not establish the host stream")
    .expect("exec_stream_core rejected the scenario request");
    assert_eq!(exec_id, EXEC_ID);
    let initial_transport = tokio::time::timeout(WEDGE_BOUND, async {
        sim.world.host.lock().await.exec_transport_counters(sandbox)
    })
    .await
    .expect("wedge: reading initial exec transport counters");
    assert_eq!(initial_transport, Some((0, 1)));

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while !stderr
        .windows(FIRST_STDERR.len())
        .any(|window| window == FIRST_STDERR)
    {
        match next_event(
            &mut stream,
            "wedge: waiting for the incident-shaped first stderr chunk",
        )
        .await
        {
            ExecStreamEvent::Stdout(bytes) => stdout.extend(bytes),
            ExecStreamEvent::Stderr(bytes) => stderr.extend(bytes),
            ExecStreamEvent::Exit { exit_status, .. } => {
                panic!("exec exited before checkpoint severance: {exit_status:?}");
            }
        }
    }

    tokio::time::timeout(WEDGE_BOUND, sim.periodic_checkpoint(session))
        .await
        .expect("wedge: periodic checkpoint did not finish after severing exec vsock");

    let mut gate_writer = tokio::time::timeout(
        WEDGE_BOUND,
        tokio::fs::OpenOptions::new().write(true).open(&gate),
    )
    .await
    .expect("wedge: command never opened the checkpoint FIFO")
    .expect("open checkpoint FIFO writer");
    tokio::time::timeout(WEDGE_BOUND, gate_writer.write_all(b"checkpoint complete\n"))
        .await
        .expect("wedge: releasing the post-checkpoint subprocess gate")
        .expect("write checkpoint FIFO");
    drop(gate_writer);

    let exit_status = loop {
        match next_event(
            &mut stream,
            "wedge: durable host reader never re-attached to exec journal",
        )
        .await
        {
            ExecStreamEvent::Stdout(bytes) => stdout.extend(bytes),
            ExecStreamEvent::Stderr(bytes) => stderr.extend(bytes),
            ExecStreamEvent::Exit { exit_status, .. } => break exit_status,
        }
    };
    assert_eq!(exit_status, Some(7));
    assert_eq!(stdout, b"stdout-before\nstdout-after\n");
    assert_eq!(
        stderr,
        b"Cloning into '/workspace/engrams'...\nstderr-after\n"
    );

    let after_exit = tokio::time::timeout(WEDGE_BOUND, stream.next())
        .await
        .expect("wedge: coordinator stream did not close after terminal Exit");
    assert!(
        after_exit.is_none(),
        "stream emitted a duplicate frame after Exit"
    );

    let marker_bytes = tokio::time::timeout(WEDGE_BOUND, tokio::fs::read(&marker))
        .await
        .expect("wedge: spawn marker read never completed")
        .expect("spawn marker exists");
    assert_eq!(
        marker_bytes, b"spawn\n",
        "attach-or-start spawned the command more than once"
    );
    let final_transport = tokio::time::timeout(WEDGE_BOUND, async {
        sim.world.host.lock().await.exec_transport_counters(sandbox)
    })
    .await
    .expect("wedge: reading final exec transport counters");
    assert_eq!(
        final_transport,
        Some((1, 2)),
        "checkpoint must sever once and force exactly one fresh real agentd connection"
    );

    let events = sim
        .world
        .meta
        .with_db(|db| db.session_events.get(&session).cloned().unwrap_or_default());
    let started: Vec<_> = events
        .iter()
        .filter(|event| {
            event.kind == "exec_started" && event.payload["exec_id"].as_str() == Some(EXEC_ID)
        })
        .collect();
    assert_eq!(
        started.len(),
        1,
        "coordinator must persist exactly one exec_started: {events:?}"
    );
    let completed: Vec<_> = events
        .iter()
        .filter(|event| {
            event.kind == "exec_completed" && event.payload["exec_id"].as_str() == Some(EXEC_ID)
        })
        .collect();
    assert_eq!(
        completed.len(),
        1,
        "the incident's missing exec_completed fact must land exactly once: {events:?}"
    );
    assert_eq!(completed[0].payload["exit_status"].as_i64(), Some(7));
}
