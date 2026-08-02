//! ADR 0103 failure-matrix row 12, guest half: the reference consumer is a
//! non-tty `git clone`, which prints NOTHING until it finishes. A silent
//! exec gives the reader no output to prove liveness with. The reader stays
//! in `read_msg` for the entire run. Two back-to-back checkpoints end two
//! consecutive connections. The driver must re-attach each time and deliver
//! the real exit with exactly one spawn and no fabricated output.
//!
//! Same composition discipline as `checkpoint_severs_exec_mid_stream`: real
//! coordinator `exec_stream_core`, real FC protocol driver, real agentd
//! journal handler; only the severed vsock transport is modeled. Real Tokio
//! time (real subprocess I/O); FIFOs order every step — the only polling is
//! a wedge-bounded wait on the transport counters between the re-attach and
//! the second checkpoint.

use std::collections::HashMap;
use std::time::Duration;

use engram_coordinator::api::exec::{exec_stream_core, ExecRequest, ExecStreamEvent};
use engram_core::types::session::SessionState;
use engram_dst_cosim::Cosim;
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

const WEDGE_BOUND: Duration = Duration::from_secs(20);
const EXEC_ID: &str = "exec:cosim:silent-clone";

async fn release(gate: &std::path::Path, label: &str) {
    let mut writer = tokio::time::timeout(
        WEDGE_BOUND,
        tokio::fs::OpenOptions::new().write(true).open(gate),
    )
    .await
    .unwrap_or_else(|_| panic!("wedge: command never opened {label}"))
    .unwrap_or_else(|error| panic!("open {label} writer: {error}"));
    tokio::time::timeout(WEDGE_BOUND, writer.write_all(b"go\n"))
        .await
        .unwrap_or_else(|_| panic!("wedge: releasing {label}"))
        .unwrap_or_else(|error| panic!("write {label}: {error}"));
}

async fn await_transport(sim: &Cosim, sandbox: engram_core::SandboxId, want: (u64, u64)) {
    tokio::time::timeout(WEDGE_BOUND, async {
        loop {
            let counters = sim.world.host.lock().await.exec_transport_counters(sandbox);
            if counters == Some(want) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("wedge: transport counters never reached {want:?}"));
}

#[tokio::test]
async fn silent_exec_survives_two_checkpoints_and_delivers_its_exit() {
    let mut sim = tokio::time::timeout(WEDGE_BOUND, Cosim::new(0x0103_51E7))
        .await
        .expect("wedge: constructing the co-sim world");
    let session = tokio::time::timeout(WEDGE_BOUND, sim.boot_session())
        .await
        .expect("wedge: boot_session did not reach its terminal step");
    assert_eq!(
        tokio::time::timeout(WEDGE_BOUND, sim.session_state(session))
            .await
            .expect("wedge: reading the booted session state"),
        Some(SessionState::Active)
    );
    let sandbox = tokio::time::timeout(WEDGE_BOUND, sim.sandbox_of(session))
        .await
        .expect("wedge: reading the Active session's sandbox")
        .expect("Active session is bound to a sandbox");

    let temp = tempfile::tempdir().expect("scenario tempdir");
    let marker = temp.path().join("spawn-marker");
    let spawn_signal = temp.path().join("spawn-signal");
    let gate_one = temp.path().join("gate-one");
    let gate_two = temp.path().join("gate-two");
    for fifo in [&spawn_signal, &gate_one, &gate_two] {
        let status = std::process::Command::new("mkfifo")
            .arg(fifo)
            .status()
            .expect("spawn mkfifo");
        assert!(status.success(), "mkfifo failed with {status}");
    }

    // Completely silent: no stdout, no stderr, ever. The spawn handshake and
    // both checkpoint windows are ordered by FIFOs.
    let script = concat!(
        "printf 'spawn\\n' >> \"$1\"\n",
        "printf 'up\\n' > \"$2\"\n",
        "IFS= read -r _ < \"$3\"\n",
        "IFS= read -r _ < \"$4\"\n",
        "exit 0\n",
    );
    let request = ExecRequest {
        command: None,
        argv: Some(vec![
            "sh".into(),
            "-c".into(),
            script.into(),
            "cosim-silent-clone".into(),
            marker.to_string_lossy().into_owned(),
            spawn_signal.to_string_lossy().into_owned(),
            gate_one.to_string_lossy().into_owned(),
            gate_two.to_string_lossy().into_owned(),
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

    // The spawn handshake: opening the FIFO for read blocks until the
    // command (already running) opens it for write.
    let signal = tokio::time::timeout(WEDGE_BOUND, tokio::fs::read(&spawn_signal))
        .await
        .expect("wedge: command never signalled spawn")
        .expect("read spawn signal");
    assert_eq!(signal, b"up\n");

    // Checkpoint #1 lands mid-silence: the reader is parked in read_msg with
    // no buffered frame to win the biased race.
    tokio::time::timeout(WEDGE_BOUND, sim.periodic_checkpoint(session))
        .await
        .expect("wedge: first checkpoint did not finish");
    // The driver re-attaches (connection 2). Only then is there a live
    // connection for checkpoint #2 to sever.
    await_transport(&sim, sandbox, (1, 2)).await;

    tokio::time::timeout(WEDGE_BOUND, sim.periodic_checkpoint(session))
        .await
        .expect("wedge: second checkpoint did not finish");
    await_transport(&sim, sandbox, (2, 3)).await;

    // Release the command; the exit must arrive over connection 3.
    release(&gate_one, "gate one").await;
    release(&gate_two, "gate two").await;

    let event = tokio::time::timeout(WEDGE_BOUND, stream.next())
        .await
        .expect("wedge: no frame after double severance — the silent-exec hang is back")
        .expect("stream ended without terminal Exit")
        .expect("coordinator stream error");
    match event {
        ExecStreamEvent::Exit { exit_status, .. } => assert_eq!(exit_status, Some(0)),
        other => panic!("a silent exec must produce no output frames, got {other:?}"),
    }
    let after_exit = tokio::time::timeout(WEDGE_BOUND, stream.next())
        .await
        .expect("wedge: coordinator stream did not close after terminal Exit");
    assert!(after_exit.is_none(), "duplicate frame after Exit");

    let marker_bytes = tokio::time::timeout(WEDGE_BOUND, tokio::fs::read(&marker))
        .await
        .expect("wedge: spawn marker read never completed")
        .expect("spawn marker exists");
    assert_eq!(
        marker_bytes, b"spawn\n",
        "attach-or-start spawned the command more than once across two severances"
    );

    let events = sim
        .world
        .meta
        .with_db(|db| db.session_events.get(&session).cloned().unwrap_or_default());
    let started = events
        .iter()
        .filter(|event| {
            event.kind == "exec_started" && event.payload["exec_id"].as_str() == Some(EXEC_ID)
        })
        .count();
    let completed: Vec<_> = events
        .iter()
        .filter(|event| {
            event.kind == "exec_completed" && event.payload["exec_id"].as_str() == Some(EXEC_ID)
        })
        .collect();
    assert_eq!(started, 1, "exactly one exec_started: {events:?}");
    assert_eq!(completed.len(), 1, "exactly one exec_completed: {events:?}");
    assert_eq!(completed[0].payload["exit_status"].as_i64(), Some(0));
}
