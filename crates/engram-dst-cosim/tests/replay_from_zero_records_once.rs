//! ADR 0103, two properties driven through the REAL guest journal (the unit
//! tests for both use a scripted stub host — precisely the layer whose real
//! behavior matters here):
//!
//! 1. **Observation-independent recording, orchestrator-restart shape.** A
//!    caller receives part of the output, dies (drops the stream), and a
//!    fresh call re-attaches from offset ZERO — the real journal replays
//!    everything as its own chunking decides, the replay straddles the
//!    recorded high-water mark, and the persisted stdout rows must still
//!    reconstruct the output exactly once.
//! 2. **Refusals are terminal and row-free end-to-end.** A second command
//!    under the same ticket crosses real agentd (first-writer-wins), the
//!    real FC protocol driver, and the real coordinator core as a
//!    `Refused` frame — surfacing as a non-retryable "exec refused" error
//!    with zero new lifecycle or output rows.

use std::collections::HashMap;
use std::fmt::Debug;
use std::time::Duration;

use engram_coordinator::api::exec::{exec_stream_core, ExecRequest, ExecStreamEvent};
use engram_core::types::session::SessionState;
use engram_dst_cosim::Cosim;
use futures_util::{Stream, StreamExt};
use tokio::io::AsyncWriteExt;

const WEDGE_BOUND: Duration = Duration::from_secs(20);
const EXEC_ID: &str = "exec:cosim:replay-from-zero";

async fn next_event<S, E>(stream: &mut S, wedge: &str) -> Result<ExecStreamEvent, E>
where
    S: Stream<Item = Result<ExecStreamEvent, E>> + Unpin,
    E: Debug,
{
    tokio::time::timeout(WEDGE_BOUND, stream.next())
        .await
        .unwrap_or_else(|_| panic!("{wedge}: timed out"))
        .unwrap_or_else(|| panic!("{wedge}: stream ended unexpectedly"))
}

fn request(argv: Vec<String>, stdout_offset: Option<u64>) -> ExecRequest {
    ExecRequest {
        command: None,
        argv: Some(argv),
        env: HashMap::new(),
        workdir: None,
        timeout_secs: None,
        exec_id: Some(EXEC_ID.into()),
        stdout_offset,
        stderr_offset: stdout_offset,
        wake: Some(false),
    }
}

#[tokio::test]
async fn replay_from_zero_records_rows_once_and_a_refusal_is_terminal() {
    let mut sim = tokio::time::timeout(WEDGE_BOUND, Cosim::new(0x0103_0EC0))
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

    let temp = tempfile::tempdir().expect("scenario tempdir");
    let gate = temp.path().join("restart-gate");
    let mkfifo = std::process::Command::new("mkfifo")
        .arg(&gate)
        .status()
        .expect("spawn mkfifo");
    assert!(mkfifo.success(), "mkfifo failed with {mkfifo}");

    // "alpha" lands before the caller dies; "beta" + exit 5 only after the
    // gate releases, i.e. only after the restarted caller re-attaches.
    let script = concat!(
        "printf 'alpha'\n",
        "IFS= read -r _ < \"$1\"\n",
        "printf 'beta'\n",
        "exit 5\n",
    );
    let argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        script.to_string(),
        "cosim-replay".to_string(),
        gate.to_string_lossy().into_owned(),
    ];

    // Attach 1: consume "alpha", then drop the stream mid-exec — the
    // orchestrator process dying. (No checkpoint needed: caller death is
    // its own severance.)
    let (_exec_id, mut first) = tokio::time::timeout(
        WEDGE_BOUND,
        exec_stream_core(&sim.world.state, session, request(argv.clone(), None)),
    )
    .await
    .expect("wedge: first exec_stream_core call")
    .expect("first attach accepted");
    let mut seen = Vec::new();
    while seen != b"alpha" {
        match next_event(&mut first, "wedge: waiting for pre-restart output")
            .await
            .expect("healthy first attach")
        {
            ExecStreamEvent::Stdout(bytes) => seen.extend(bytes),
            ExecStreamEvent::Stderr(bytes) => {
                panic!("unexpected stderr: {:?}", String::from_utf8_lossy(&bytes))
            }
            ExecStreamEvent::Exit { exit_status, .. } => {
                panic!("exec exited before the caller restart: {exit_status:?}")
            }
        }
    }
    drop(first);

    // The command may proceed only once the replacement caller exists.
    let (_exec_id, mut second) = tokio::time::timeout(
        WEDGE_BOUND,
        exec_stream_core(&sim.world.state, session, request(argv.clone(), Some(0))),
    )
    .await
    .expect("wedge: replacement exec_stream_core call")
    .expect("replacement attach accepted");
    let mut gate_writer = tokio::time::timeout(
        WEDGE_BOUND,
        tokio::fs::OpenOptions::new().write(true).open(&gate),
    )
    .await
    .expect("wedge: command never opened the restart gate")
    .expect("open restart gate writer");
    tokio::time::timeout(WEDGE_BOUND, gate_writer.write_all(b"restarted\n"))
        .await
        .expect("wedge: releasing the restart gate")
        .expect("write restart gate");
    drop(gate_writer);

    // Amnesiac cursor: the replacement asked for byte 0, so the full
    // output must STREAM, replay and tail alike.
    let mut streamed = Vec::new();
    let exit_status = loop {
        match next_event(&mut second, "wedge: draining the replacement attach")
            .await
            .expect("healthy replacement attach")
        {
            ExecStreamEvent::Stdout(bytes) => streamed.extend(bytes),
            ExecStreamEvent::Stderr(bytes) => {
                panic!("unexpected stderr: {:?}", String::from_utf8_lossy(&bytes))
            }
            ExecStreamEvent::Exit { exit_status, .. } => break exit_status,
        }
    };
    assert_eq!(exit_status, Some(5));
    assert_eq!(
        streamed, b"alphabeta",
        "the caller must receive the full replay"
    );

    let stdout_rows = |sim: &Cosim| -> Vec<(String, u64, u64)> {
        sim.world.meta.with_db(|db| {
            db.session_events
                .get(&session)
                .into_iter()
                .flatten()
                .filter(|event| {
                    event.kind == "stdout" && event.payload["exec_id"].as_str() == Some(EXEC_ID)
                })
                .map(|event| {
                    (
                        event.payload["chunk"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        event.payload["bytes_start"].as_u64().unwrap_or(u64::MAX),
                        event.payload["bytes_end"].as_u64().unwrap_or(u64::MAX),
                    )
                })
                .collect()
        })
    };
    let rows = stdout_rows(&sim);
    let assembled: String = rows.iter().map(|(chunk, _, _)| chunk.as_str()).collect();
    assert_eq!(
        assembled, "alphabeta",
        "recorded rows must reconstruct the output exactly once despite the \
         from-zero replay; got {rows:?}"
    );
    let mut expected_next = 0;
    for (_, start, end) in &rows {
        assert_eq!(
            *start, expected_next,
            "row ranges must tile the stream without gap or overlap: {rows:?}"
        );
        expected_next = *end;
    }
    assert_eq!(expected_next, 9, "ranges must cover all 9 bytes: {rows:?}");

    let lifecycle = |kind: &str| {
        sim.world.meta.with_db(|db| {
            db.session_events
                .get(&session)
                .into_iter()
                .flatten()
                .filter(|event| {
                    event.kind == kind && event.payload["exec_id"].as_str() == Some(EXEC_ID)
                })
                .count()
        })
    };
    assert_eq!(lifecycle("exec_started"), 1);
    assert_eq!(lifecycle("exec_completed"), 1);

    // A different command under the same ticket: real agentd refuses
    // (first-writer-wins) and the refusal must be a terminal, row-free
    // error at the coordinator.
    let hostile = vec![
        "sh".to_string(),
        "-c".to_string(),
        "echo hijack".to_string(),
    ];
    let (_exec_id, mut refused) = tokio::time::timeout(
        WEDGE_BOUND,
        exec_stream_core(&sim.world.state, session, request(hostile, Some(0))),
    )
    .await
    .expect("wedge: refusal exec_stream_core call")
    .expect("refusal attach establishes a stream before the terminal error");
    match next_event(&mut refused, "wedge: waiting for the refusal terminal").await {
        Err(error) => {
            let message = format!("{error:?}");
            assert!(
                message.contains("exec refused"),
                "the refusal must surface as a non-retryable exec-refused error, got {message}"
            );
        }
        Ok(other) => panic!("expected the refusal terminal, got {other:?}"),
    }
    let after = tokio::time::timeout(WEDGE_BOUND, refused.next())
        .await
        .expect("wedge: refusal stream must close after its terminal error");
    assert!(after.is_none(), "no frames may follow the refusal terminal");

    assert_eq!(
        stdout_rows(&sim).len(),
        rows.len(),
        "a refusal must not add output rows"
    );
    assert_eq!(
        lifecycle("exec_started"),
        1,
        "a refusal must not add lifecycle rows"
    );
    assert_eq!(lifecycle("exec_completed"), 1);
}
