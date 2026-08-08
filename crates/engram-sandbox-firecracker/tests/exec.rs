//! End-to-end test of `FirecrackerBackend::exec_stream_via_agent_socket`
//! against an in-process `engram-agentd` listener.
//!
//! This deliberately skips Firecracker: the real production path
//! (host → vsock UDS → guest agent) is the same wire protocol on top
//! of the same `tokio::net::UnixStream`, so exercising the protocol
//! over a plain UDS exercises everything except the FC vsock proxy.
//! The `#[ignore]`'d `exec_real_vm` test runs this same wire protocol
//! through a real microVM (rootfs packed by the fixture baker, agentd
//! staged via its bundle slot); this UDS test is the highest-fidelity
//! one that runs on either macOS or Linux without KVM.

#![cfg(unix)]

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use engram_agentd::exec_journal::ExecJournal;
use engram_agentd::{serve_connection_with_journal, HarnessSupervisor};
use engram_core::types::ids::SandboxId;
use engram_core::types::sandbox::{ExecRequest, SessionFileSpec, SessionFileStream, WriteFileSpec};
use engram_sandbox_firecracker::FirecrackerBackend;
use futures::{stream, StreamExt};
use sha2::{Digest, Sha256};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

use common::drain;

/// Spawn an `engram-agentd`-shaped listener at `socket`. Returns a
/// `JoinHandle` so callers can `.abort()` it after the test. The
/// listener accepts one connection per exec, just like the real
/// agent's `main.rs`.
async fn spawn_test_agent(socket: PathBuf) -> JoinHandle<()> {
    let listener = UnixListener::bind(&socket).expect("bind UDS");
    // One supervisor + one CA-cert installer across all accepted
    // connections — mirrors the real agent's main.rs shape (ADR
    // 0021 P1.1 added the cacerts installer arg).
    let supervisor = HarnessSupervisor::new();
    let cacerts = std::sync::Arc::new(engram_agentd::CaCertInstaller::for_tests());
    // These in-process tests run from a Rust test harness binary, not the
    // `engram-agentd` binary that implements the hidden durable wrapper mode.
    // Force the explicit journal-failure path and exercise stage-1 streaming;
    // exec_journal_crash.rs covers durable attach, and exec_real_vm covers the
    // real wrapper executable.
    let journal = std::sync::Arc::new(ExecJournal::new("/dev/null/execs"));
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let sup = supervisor.clone();
                    let ca = cacerts.clone();
                    let journal = journal.clone();
                    tokio::spawn(async move {
                        let _ = serve_connection_with_journal(stream, None, sup, ca, journal).await;
                    });
                }
                Err(_) => return,
            }
        }
    })
}

#[tokio::test]
async fn exec_stream_via_agent_socket_round_trips_stdout_and_exit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("agent.sock");
    let agent = spawn_test_agent(socket.clone()).await;

    let stream = FirecrackerBackend::exec_stream_via_agent_socket(
        SandboxId::new(),
        &socket,
        ExecRequest {
            command: vec!["sh".into(), "-c".into(), "printf hello".into()],
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout: None,
            exec_id: None,
            stdout_offset: None,
            stderr_offset: None,
            wake: None,
        },
    )
    .await
    .expect("exec_stream_via_agent_socket");

    let (stdout, stderr, exit) = drain(stream.events).await;
    assert_eq!(stdout, b"hello");
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert_eq!(exit, Some(0));

    agent.abort();
}

#[tokio::test]
async fn write_files_via_agent_socket_reports_each_file_result() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("agent.sock");
    let agent = spawn_test_agent(socket.clone()).await;
    let written = dir.path().join("written.txt");

    let results = FirecrackerBackend::write_files_via_agent_socket(
        SandboxId::new(),
        &socket,
        vec![
            // Writing over an existing directory fails inside agentd, proving
            // a per-file failure does not abort the rest of the batch.
            WriteFileSpec {
                path: dir.path().to_string_lossy().into_owned(),
                content: b"cannot replace a directory".to_vec(),
                mode: None,
            },
            WriteFileSpec {
                path: written.to_string_lossy().into_owned(),
                content: b"staged".to_vec(),
                mode: Some(0o600),
            },
        ],
    )
    .await
    .expect("write_files_via_agent_socket");

    assert_eq!(results.len(), 2);
    assert!(!results[0].ok, "directory write should fail");
    assert!(results[0].error.is_some());
    assert!(results[1].ok, "second write failed: {:?}", results[1]);
    assert_eq!(tokio::fs::read(&written).await.unwrap(), b"staged");

    agent.abort();
}

#[tokio::test]
async fn session_file_stream_exceeds_old_unary_limit_and_round_trips_exactly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("agent.sock");
    let agent = spawn_test_agent(socket.clone()).await;
    let destination = dir.path().join("large-upload.bin");
    let path = destination.to_string_lossy().into_owned();
    let expected = vec![0xa5; 3 * 1024 * 1024 + 1];
    let sha256 = Sha256::digest(&expected)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let chunks = expected
        .chunks(64 * 1024)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect::<Vec<_>>();
    FirecrackerBackend::upload_file_via_agent_socket(
        &socket,
        SessionFileSpec {
            path: path.clone(),
            size_bytes: expected.len() as u64,
            sha256: sha256.clone(),
        },
        Box::pin(stream::iter(chunks)) as SessionFileStream,
    )
    .await
    .expect("stream upload");

    let (metadata, mut stream) = FirecrackerBackend::read_file_via_agent_socket(&socket, path)
        .await
        .expect("stream read");
    let mut actual = Vec::new();
    while let Some(chunk) = stream.next().await {
        actual.extend_from_slice(&chunk.expect("read chunk"));
    }
    assert_eq!(metadata.size_bytes, expected.len() as u64);
    assert_eq!(metadata.sha256, sha256);
    assert_eq!(actual, expected);
    agent.abort();
}

#[tokio::test]
async fn exec_stream_propagates_nonzero_exit_status() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("agent.sock");
    let agent = spawn_test_agent(socket.clone()).await;

    let stream = FirecrackerBackend::exec_stream_via_agent_socket(
        SandboxId::new(),
        &socket,
        ExecRequest {
            command: vec!["sh".into(), "-c".into(), "exit 7".into()],
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout: None,
            exec_id: None,
            stdout_offset: None,
            stderr_offset: None,
            wake: None,
        },
    )
    .await
    .unwrap();
    let (_, _, exit) = drain(stream.events).await;
    assert_eq!(exit, Some(7));

    agent.abort();
}

#[tokio::test]
async fn exec_stream_separates_stdout_and_stderr() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("agent.sock");
    let agent = spawn_test_agent(socket.clone()).await;

    let stream = FirecrackerBackend::exec_stream_via_agent_socket(
        SandboxId::new(),
        &socket,
        ExecRequest {
            command: vec![
                "sh".into(),
                "-c".into(),
                "printf out; printf err >&2".into(),
            ],
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout: None,
            exec_id: None,
            stdout_offset: None,
            stderr_offset: None,
            wake: None,
        },
    )
    .await
    .unwrap();
    let (stdout, stderr, exit) = drain(stream.events).await;
    assert_eq!(stdout, b"out");
    assert_eq!(stderr, b"err");
    assert_eq!(exit, Some(0));

    agent.abort();
}

#[tokio::test]
async fn exec_stream_pipes_stdin_to_child() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("agent.sock");
    let agent = spawn_test_agent(socket.clone()).await;

    let stream = FirecrackerBackend::exec_stream_via_agent_socket(
        SandboxId::new(),
        &socket,
        ExecRequest {
            command: vec!["cat".into()],
            stdin: Some(b"piped".to_vec()),
            env: HashMap::new(),
            workdir: None,
            timeout: None,
            exec_id: None,
            stdout_offset: None,
            stderr_offset: None,
            wake: None,
        },
    )
    .await
    .unwrap();
    let (stdout, _, exit) = drain(stream.events).await;
    assert_eq!(stdout, b"piped");
    assert_eq!(exit, Some(0));

    agent.abort();
}

#[tokio::test]
async fn exec_stream_translates_timeout_to_kill_and_exit_none() {
    // Locks in the protocol contract: timeout-killed exec surfaces
    // as Exit(None), not a synthetic error. Ensures consumers don't
    // hang forever waiting on the next event.
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("agent.sock");
    let agent = spawn_test_agent(socket.clone()).await;

    let stream = FirecrackerBackend::exec_stream_via_agent_socket(
        SandboxId::new(),
        &socket,
        ExecRequest {
            command: vec!["sleep".into(), "10".into()],
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout: Some(Duration::from_millis(50)),
            exec_id: None,
            stdout_offset: None,
            stderr_offset: None,
            wake: None,
        },
    )
    .await
    .unwrap();
    let (_, _, exit) = drain(stream.events).await;
    assert_eq!(exit, None, "timeout must surface as Exit(None)");

    agent.abort();
}

#[tokio::test]
async fn exec_stream_surfaces_connect_error_when_agent_socket_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("does-not-exist.sock");
    let res = FirecrackerBackend::exec_stream_via_agent_socket(
        SandboxId::new(),
        &socket,
        ExecRequest {
            command: vec!["true".into()],
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout: None,
            exec_id: None,
            stdout_offset: None,
            stderr_offset: None,
            wake: None,
        },
    )
    .await;
    let err = res.expect_err("expected error");
    let msg = format!("{err:?}");
    assert!(
        msg.to_lowercase().contains("connect"),
        "error should mention connect failure: {msg}",
    );
}
