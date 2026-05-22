//! End-to-end test of `FirecrackerBackend::exec_stream_via_agent_socket`
//! against an in-process `engram-agentd` listener.
//!
//! This deliberately skips Firecracker: the real production path
//! (host → vsock UDS → guest agent) is the same wire protocol on top
//! of the same `tokio::net::UnixStream`, so exercising the protocol
//! over a plain UDS exercises everything except the FC vsock proxy.
//! When the image baker can produce a rootfs with `engram-agentd`
//! embedded (next slice), an `#[ignore]` test will run this through
//! a real microVM. Until then, this is the highest-fidelity test we
//! can run on either macOS or Linux.

#![cfg(unix)]

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use engram_agentd::{serve_connection, HarnessSupervisor};
use engram_core::types::ids::SandboxId;
use engram_core::types::sandbox::ExecRequest;
use engram_sandbox_firecracker::FirecrackerBackend;
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

use common::drain;

/// Spawn an `engram-agentd`-shaped listener at `socket`. Returns a
/// `JoinHandle` so callers can `.abort()` it after the test. The
/// listener accepts one connection per exec, just like the real
/// agent's `main.rs`.
async fn spawn_test_agent(socket: PathBuf) -> JoinHandle<()> {
    let listener = UnixListener::bind(&socket).expect("bind UDS");
    // One supervisor across all accepted connections — mirrors the
    // real agent's main.rs shape.
    let supervisor = HarnessSupervisor::new();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let sup = supervisor.clone();
                    tokio::spawn(async move {
                        let _ = serve_connection(stream, None, sup).await;
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
