//! Per-connection exec handler.
//!
//! Concurrency model: stdout and stderr are read in parallel tasks
//! that send `WireExecEvent`s through an `mpsc` channel; a single
//! writer task drains the channel and frames each event onto the
//! connection. This keeps `WireExecEvent::Exit` strictly *last* on
//! the wire (we close the channel only after both readers drain),
//! and avoids the lock-around-the-stream dance that two writers
//! would otherwise need.

use std::io;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{split, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::proto::{read_msg, write_msg, WireExecEvent, WireExecRequest};

/// Channel buffer between the stdout/stderr readers and the writer.
/// Modest depth: under backpressure we'd rather slow the child than
/// buffer megabytes of stdout in agent memory.
const EVENT_BUF: usize = 64;

/// Read size per stream. 8 KiB is a reasonable balance between syscall
/// overhead and frame granularity (a single frame can carry up to 8 KiB
/// of stdout, which keeps the JSON framing comparable in chunkiness).
const READ_BUF_BYTES: usize = 8 * 1024;

/// Drive one connection: read a `WireExecRequest`, run the child,
/// stream events, close. All errors are surfaced as `io::Error` —
/// the caller decides whether to log them and move on (the agent's
/// accept loop) or treat them as fatal (a test).
pub async fn serve_connection<S>(stream: S) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (mut reader, writer) = split(stream);
    let req: WireExecRequest = read_msg(&mut reader).await?;
    if req.command.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WireExecRequest.command is empty",
        ));
    }

    let mut cmd = Command::new(&req.command[0]);
    cmd.args(&req.command[1..]);
    for (k, v) in &req.env {
        cmd.env(k, v);
    }
    if let Some(wd) = &req.workdir {
        cmd.current_dir(wd);
    }
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if req.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        // SIGKILL on drop in case any later .await? bails before we
        // reach child.wait — we'd otherwise leak the process.
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("spawn {:?}: {e}", req.command[0])))?;

    // stdin is fire-and-forget: drain the buffer, then close.
    if let Some(bytes) = req.stdin {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&bytes).await.ok();
            // Dropping closes the pipe — child sees EOF on stdin.
        }
    }

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let (tx, mut rx) = mpsc::channel::<WireExecEvent>(EVENT_BUF);

    let stdout_task = tokio::spawn(forward_stream(stdout, tx.clone(), Stream::Out));
    let stderr_task = tokio::spawn(forward_stream(stderr, tx.clone(), Stream::Err));

    // Single writer task owns the connection's write half and the
    // bincode framing — keeps event ordering well-defined.
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(ev) = rx.recv().await {
            write_msg(&mut writer, &ev).await?;
        }
        Ok::<_, io::Error>(())
    });

    // Wait on the child (with optional timeout). On timeout, SIGKILL
    // and emit Exit(None) so the host sees a definitive end.
    let exit_status = match req.timeout_ms {
        Some(ms) => match tokio::time::timeout(Duration::from_millis(ms), child.wait()).await {
            Ok(res) => res?,
            Err(_) => {
                // Best-effort kill; if the child already raced to
                // exit, kill returns Ok or NotFound, both fine.
                let _ = child.kill().await;
                let _ = child.wait().await;
                drop_send(&tx, WireExecEvent::Exit(None)).await;
                drop(tx);
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                writer_task.await.unwrap_or(Ok(()))?;
                return Ok(());
            }
        },
        None => child.wait().await?,
    };

    // Make sure both pipe-drainers finish before sending Exit so the
    // host sees all output that preceded the exit.
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    drop_send(&tx, WireExecEvent::Exit(exit_status.code())).await;
    drop(tx);

    // Surface a writer error (e.g. host disconnected mid-stream) so
    // a test can fail; the agent's accept loop just logs it.
    writer_task.await.unwrap_or(Ok(()))?;
    Ok(())
}

#[derive(Clone, Copy)]
enum Stream {
    Out,
    Err,
}

/// Read a child stream into 8KiB chunks and ship each as a single
/// `WireExecEvent`. EOF on the child stream ends the loop quietly.
async fn forward_stream<R>(mut r: R, tx: mpsc::Sender<WireExecEvent>, kind: Stream)
where
    R: AsyncRead + Unpin,
{
    let mut buf = vec![0u8; READ_BUF_BYTES];
    loop {
        match r.read(&mut buf).await {
            Ok(0) => return,
            Ok(n) => {
                let chunk = buf[..n].to_vec();
                let ev = match kind {
                    Stream::Out => WireExecEvent::Stdout(chunk),
                    Stream::Err => WireExecEvent::Stderr(chunk),
                };
                if tx.send(ev).await.is_err() {
                    // Writer task is gone (host closed connection).
                    // Stop pulling from the child — child will SIGPIPE
                    // when it next writes to a closed stdout.
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// Best-effort send. Drops the event if the writer task is gone —
/// the connection is already in a broken state, no point bubbling
/// the error up further.
async fn drop_send(tx: &mpsc::Sender<WireExecEvent>, ev: WireExecEvent) {
    let _ = tx.send(ev).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::io::duplex;

    /// In-memory `serve_connection` round-trip. We feed a request in
    /// on one end of a duplex pipe, run the handler against the other,
    /// then drain events on the original side.
    async fn run_against(req: WireExecRequest) -> Vec<WireExecEvent> {
        let (mut client, server) = duplex(64 * 1024);

        // Server side: handler reads request, runs cmd, writes events.
        let server_task = tokio::spawn(async move { serve_connection(server).await });

        // Client side: send request, then read events until EOF.
        write_msg(&mut client, &req).await.unwrap();
        let mut events = Vec::new();
        loop {
            match read_msg::<_, WireExecEvent>(&mut client).await {
                Ok(ev) => {
                    let is_exit = matches!(ev, WireExecEvent::Exit(_));
                    events.push(ev);
                    if is_exit {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => panic!("read_msg: {e}"),
            }
        }
        let _ = server_task.await.unwrap();
        events
    }

    #[tokio::test]
    async fn echo_emits_stdout_and_clean_exit() {
        let evs = run_against(WireExecRequest {
            command: vec!["sh".into(), "-c".into(), "printf hello".into()],
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout_ms: None,
        })
        .await;
        let stdout: Vec<u8> = evs
            .iter()
            .filter_map(|e| match e {
                WireExecEvent::Stdout(b) => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(stdout, b"hello");
        // Last event must be a clean exit
        assert!(matches!(evs.last(), Some(WireExecEvent::Exit(Some(0))),));
    }

    #[tokio::test]
    async fn nonzero_exit_status_is_propagated() {
        let evs = run_against(WireExecRequest {
            command: vec!["sh".into(), "-c".into(), "exit 7".into()],
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout_ms: None,
        })
        .await;
        assert!(matches!(evs.last(), Some(WireExecEvent::Exit(Some(7)))));
    }

    #[tokio::test]
    async fn stderr_is_routed_to_stderr_events() {
        let evs = run_against(WireExecRequest {
            command: vec!["sh".into(), "-c".into(), "printf err >&2".into()],
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout_ms: None,
        })
        .await;
        let stderr: Vec<u8> = evs
            .iter()
            .filter_map(|e| match e {
                WireExecEvent::Stderr(b) => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(stderr, b"err");
    }

    #[tokio::test]
    async fn stdin_bytes_reach_the_child() {
        // `cat` echoes its stdin to stdout; we send "ping" and expect
        // it back as a Stdout event.
        let evs = run_against(WireExecRequest {
            command: vec!["cat".into()],
            stdin: Some(b"ping".to_vec()),
            env: HashMap::new(),
            workdir: None,
            timeout_ms: None,
        })
        .await;
        let stdout: Vec<u8> = evs
            .iter()
            .filter_map(|e| match e {
                WireExecEvent::Stdout(b) => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(stdout, b"ping");
        assert!(matches!(evs.last(), Some(WireExecEvent::Exit(Some(0)))));
    }

    #[tokio::test]
    async fn env_vars_reach_the_child() {
        let evs = run_against(WireExecRequest {
            command: vec!["sh".into(), "-c".into(), "printf $ENGRAM_TEST".into()],
            stdin: None,
            env: HashMap::from([("ENGRAM_TEST".into(), "wired".into())]),
            workdir: None,
            timeout_ms: None,
        })
        .await;
        let stdout: Vec<u8> = evs
            .iter()
            .filter_map(|e| match e {
                WireExecEvent::Stdout(b) => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(stdout, b"wired");
    }

    #[tokio::test]
    async fn timeout_kills_long_running_child_and_exits_with_none() {
        let evs = run_against(WireExecRequest {
            command: vec!["sleep".into(), "5".into()],
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout_ms: Some(50),
        })
        .await;
        // Killed by timeout: Exit(None) per the protocol.
        assert!(matches!(evs.last(), Some(WireExecEvent::Exit(None))));
    }

    #[tokio::test]
    async fn empty_command_is_rejected() {
        // Direct call — the helper expects the handler to return Ok,
        // but here the handler must error. Drive serve_connection
        // manually so we can inspect the error.
        let (mut client, server) = duplex(1024);
        let server_task = tokio::spawn(async move { serve_connection(server).await });
        write_msg(
            &mut client,
            &WireExecRequest {
                command: Vec::new(),
                stdin: None,
                env: HashMap::new(),
                workdir: None,
                timeout_ms: None,
            },
        )
        .await
        .unwrap();
        let res = server_task.await.unwrap();
        let err = res.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn missing_binary_surfaces_io_error() {
        let (mut client, server) = duplex(1024);
        let server_task = tokio::spawn(async move { serve_connection(server).await });
        write_msg(
            &mut client,
            &WireExecRequest {
                command: vec!["/this/binary/does/not/exist".into()],
                stdin: None,
                env: HashMap::new(),
                workdir: None,
                timeout_ms: None,
            },
        )
        .await
        .unwrap();
        let err = server_task.await.unwrap().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().contains("spawn"));
    }

    /// Intentionally not asserting any cross-stream interleaving —
    /// only that all bytes arrive intact and that Exit comes last.
    #[tokio::test]
    async fn stdout_and_stderr_both_arrive_with_exit_last() {
        let evs = run_against(WireExecRequest {
            command: vec![
                "sh".into(),
                "-c".into(),
                "printf out; printf err >&2".into(),
            ],
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout_ms: None,
        })
        .await;
        let mut total_out = Vec::new();
        let mut total_err = Vec::new();
        for ev in &evs[..evs.len() - 1] {
            match ev {
                WireExecEvent::Stdout(b) => total_out.extend_from_slice(b),
                WireExecEvent::Stderr(b) => total_err.extend_from_slice(b),
                WireExecEvent::Exit(_) => panic!("Exit must be the last event, not in the middle"),
            }
        }
        assert_eq!(total_out, b"out");
        assert_eq!(total_err, b"err");
        assert!(matches!(evs.last(), Some(WireExecEvent::Exit(Some(0)))));
    }
}
