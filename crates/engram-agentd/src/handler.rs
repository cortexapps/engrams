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
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{split, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::harness_supervisor::HarnessSupervisor;
use crate::proto::{
    read_msg, write_msg, WireDownloadResponse, WireExecEvent, WireHandshake, WireHandshakeAck,
    WireRequest, WireResponse, WireStatResponse,
};

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
///
/// `expected_token = None` skips the handshake entirely (back-compat
/// with the no-auth path; tests use this). `expected_token = Some(t)`
/// requires the host to send a [`WireHandshake`] with `token == t`
/// before it gets to send a [`WireExecRequest`]; mismatch returns
/// an `Unauthorized`-flavoured `io::Error` after writing the
/// rejection ack.
pub async fn serve_connection<S>(
    stream: S,
    expected_token: Option<String>,
    supervisor: Arc<HarnessSupervisor>,
    cacerts: Arc<crate::cacerts::CaCertInstaller>,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (mut reader, mut writer) = split(stream);

    if let Some(expected) = expected_token.as_deref() {
        let hs: WireHandshake = read_msg(&mut reader).await?;
        if !ct_eq(hs.token.as_bytes(), expected.as_bytes()) {
            // Send a typed rejection so the host sees a clean reason
            // rather than a connection drop. The message is
            // intentionally generic — never echo what the bad token
            // looked like.
            let ack = WireHandshakeAck {
                ok: false,
                message: Some("token mismatch".into()),
            };
            // Best-effort write; the host may have already torn the
            // connection down on its end.
            let _ = write_msg(&mut writer, &ack).await;
            tracing::warn!(
                agent_version = %hs.agent_version,
                "rejected handshake — token mismatch",
            );
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "handshake token mismatch",
            ));
        }
        let ack = WireHandshakeAck {
            ok: true,
            message: None,
        };
        write_msg(&mut writer, &ack).await?;
        tracing::debug!(
            agent_version = %hs.agent_version,
            "handshake accepted",
        );
    }

    let req: WireRequest = match read_msg(&mut reader).await {
        Ok(req) => req,
        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
            // `read_msg` already consumed the full frame (length
            // prefix + body) before bincode failed to decode it — the
            // stream is clean, so a reply is safe. This is NOT a
            // disconnect (that surfaces as `UnexpectedEof` from
            // `read_exact` and falls to the `Err(e)` arm below,
            // un-NAK'd): it's a well-framed request this agentd
            // couldn't parse, which happens when the host and guest
            // agentd disagree on the `WireRequest` enum — most often
            // because the guest's agentd is baked into its image and
            // predates a variant the host just sent (or, less
            // commonly, speaks a newer protocol than this build
            // understands).
            //
            // Without this, that skew is indistinguishable from
            // agentd crashing mid-call: prod session 8174b7aa ran a
            // dev-brain image whose agentd predated
            // `WireRequest::StartBrowser`; the unknown variant failed
            // to decode, `serve_connection` returned `Err` having
            // written zero bytes, and the host only ever logged
            // `start_browser: recv: early eof` (#567). Best-effort
            // write — the host may have already torn the connection
            // down on its end (mirrors the token-mismatch path
            // above) — then return the original error so logging is
            // unchanged.
            let resp = WireResponse::Error {
                kind: format!("{:?}", e.kind()),
                message: format!(
                    "unsupported or malformed request (agentd v{version}): {e} -- \
                     likely host/guest version skew (this guest's agentd may \
                     predate a request the host just sent, or speak an older \
                     protocol than the host expects); remedy: re-bake the \
                     image and RefreshImage the session",
                    version = env!("CARGO_PKG_VERSION"),
                ),
            };
            let _ = write_msg(&mut writer, &resp).await;
            return Err(e);
        }
        Err(e) => return Err(e),
    };
    let exec_req = match req {
        WireRequest::Exec(e) => e,
        WireRequest::Stat { path } => {
            let resp = stat_path(&path).await;
            write_msg(&mut writer, &WireResponse::Stat(resp)).await?;
            return Ok(());
        }
        WireRequest::Upload { path, bytes, mode } => {
            let resp = upload_path(&path, &bytes, mode).await;
            write_msg(&mut writer, &resp).await?;
            return Ok(());
        }
        WireRequest::Download { path } => {
            let resp = download_path(&path).await;
            write_msg(&mut writer, &resp).await?;
            return Ok(());
        }
        WireRequest::Ping => {
            write_msg(&mut writer, &WireResponse::Pong).await?;
            return Ok(());
        }
        WireRequest::Shutdown => {
            // Acknowledge and close the connection. The agent
            // process keeps running — the host pairs this verb
            // with `PUT /actions SendCtrlAltDel` on Firecracker
            // to actually stop the VM (the SendCtrlAltDel piece
            // is in Phase 6's deferred list). For now, Shutdown
            // is the agent-side handshake that says "I've
            // flushed any in-flight work and you can power me
            // down whenever".
            write_msg(&mut writer, &WireResponse::ShutdownAck).await?;
            return Ok(());
        }
        WireRequest::GuestIp => {
            let ip = read_primary_ipv4();
            write_msg(&mut writer, &WireResponse::GuestIp(ip)).await?;
            return Ok(());
        }
        WireRequest::Sync => {
            // Flush the guest page cache to the virtio-blk disk so a
            // clone-snapshot backend (VZ) captures just-written state.
            // `sync(2)` can block on slow I/O, so run it off the async
            // runtime. It cannot fail (POSIX `sync` returns void); the
            // safe `nix` wrapper keeps the crate's no-unsafe policy.
            // Linux-gated like the rest of the in-guest syscalls (nix is
            // a Linux-only dep) — agentd only ever runs in the guest, so
            // on non-Linux the reply is a no-op satisfying the wire shape.
            #[cfg(target_os = "linux")]
            let _ = tokio::task::spawn_blocking(nix::unistd::sync).await;
            write_msg(&mut writer, &WireResponse::Synced).await?;
            return Ok(());
        }
        WireRequest::StartShell { port } => {
            let port = port.unwrap_or(crate::shell::DEFAULT_TTYD_PORT);
            // The interactive shell inherits the same durable session env
            // as the harness and `/exec`, so `cargo build` in the SHELL tab
            // uses the shared sccache + sees the image's secrets.
            let resp = match crate::shell::start_shell(port, supervisor.session_env()).await {
                Ok(outcome) => WireResponse::ShellReady {
                    port: outcome.port,
                    spawned: outcome.spawned,
                },
                Err(e) => WireResponse::Error {
                    kind: format!("{:?}", e.kind()),
                    message: format!("start_shell: {e}"),
                },
            };
            write_msg(&mut writer, &resp).await?;
            return Ok(());
        }
        WireRequest::StartBrowser { port } => {
            let port = port.unwrap_or(crate::browser::DEFAULT_VNC_PORT);
            // The browser stack inherits the same durable session env as the
            // harness and `/exec`, so chromium sees the image's proxy/secret
            // env (mirrors the StartShell arm above).
            let resp = match crate::browser::start_browser(port, supervisor.session_env()).await {
                Ok(outcome) => WireResponse::BrowserReady {
                    port: outcome.port,
                    spawned: outcome.spawned,
                },
                Err(e) => WireResponse::Error {
                    kind: format!("{:?}", e.kind()),
                    message: format!("start_browser: {e}"),
                },
            };
            write_msg(&mut writer, &resp).await?;
            return Ok(());
        }
        WireRequest::StopBrowser => {
            let resp = match crate::browser::stop_browser().await {
                Ok(()) => WireResponse::BrowserStopped,
                Err(e) => WireResponse::Error {
                    kind: format!("{:?}", e.kind()),
                    message: format!("stop_browser: {e}"),
                },
            };
            write_msg(&mut writer, &resp).await?;
            return Ok(());
        }
        WireRequest::SpawnHarness(req) => {
            let resp = match supervisor.spawn(req).await {
                Ok(pid) => WireResponse::HarnessSpawned { pid },
                Err(e) => WireResponse::Error {
                    kind: format!("{:?}", e.kind()),
                    message: format!("spawn_harness: {e}"),
                },
            };
            write_msg(&mut writer, &resp).await?;
            return Ok(());
        }
        WireRequest::InstallHostCa(req) => {
            let resp = match cacerts.install(&req.cert_pem).await {
                Ok(changed) => WireResponse::InstallHostCaAck { changed },
                Err(e) => WireResponse::Error {
                    kind: format!("{:?}", e.kind()),
                    message: format!("install_host_ca: {e}"),
                },
            };
            write_msg(&mut writer, &resp).await?;
            return Ok(());
        }
    };

    let req = exec_req;
    if req.command.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "WireExecRequest.command is empty",
        ));
    }

    // Sync the guest clock to the host before spawning. A just-resumed
    // session would otherwise run this command on a clock frozen at
    // snapshot time (hours behind), breaking SigV4 (e.g. sccache → GCS),
    // token windows, and timestamps. No-op when there's no skew / no PTP.
    crate::clock::sync_now();

    let mut cmd = Command::new(&req.command[0]);
    cmd.args(&req.command[1..]);
    // Base: the durable session env agentd holds from the bind (image
    // `[env]`, secrets, PATH, session id) — identical to what the harness
    // and the interactive shell get, so `engram exec cargo build` sees the
    // same sccache/secret env an agent would. The request's own env layers
    // on top: a caller can override an image default, and per-request
    // credentials (the forge broker token) ride in here.
    let session_env = supervisor.session_env();
    for (k, v) in &session_env {
        cmd.env(k, v);
    }
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

// ---- Non-streaming verb implementations -------------------------------

async fn stat_path(path: &str) -> WireStatResponse {
    match tokio::fs::metadata(path).await {
        Ok(m) => {
            let mtime_unix = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            WireStatResponse {
                exists: true,
                size: m.len(),
                mtime_unix,
                is_dir: m.is_dir(),
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => WireStatResponse {
            exists: false,
            size: 0,
            mtime_unix: 0,
            is_dir: false,
        },
        Err(e) => {
            // Permission denied / IO errors collapse into "exists =
            // false" to keep the wire shape simple — the host can
            // upload-and-retry to discover the real story. We log
            // so an operator can still see the underlying issue.
            tracing::warn!(path, error = %e, "stat fell back to not-found");
            WireStatResponse {
                exists: false,
                size: 0,
                mtime_unix: 0,
                is_dir: false,
            }
        }
    }
}

async fn upload_path(path: &str, bytes: &[u8], mode: Option<u32>) -> WireResponse {
    let p = std::path::Path::new(path);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return wire_io_err("create parent dir", e);
            }
        }
    }
    if let Err(e) = tokio::fs::write(path, bytes).await {
        return wire_io_err("write", e);
    }
    if let Some(_mode) = mode {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perm = std::fs::Permissions::from_mode(_mode);
            if let Err(e) = tokio::fs::set_permissions(path, perm).await {
                return wire_io_err("chmod", e);
            }
        }
    }
    WireResponse::UploadOk
}

async fn download_path(path: &str) -> WireResponse {
    match tokio::fs::read(path).await {
        Ok(bytes) => {
            // Hard-cap at the framing limit so we don't emit a
            // frame the peer is required to reject. The host can
            // chunk via separate Download requests over different
            // ranges once we add a range-aware verb.
            if bytes.len() > crate::proto::MAX_MSG_BYTES {
                return WireResponse::Error {
                    kind: format!("{:?}", io::ErrorKind::InvalidData),
                    message: format!(
                        "file is {} bytes; exceeds {} MiB single-frame cap",
                        bytes.len(),
                        crate::proto::MAX_MSG_BYTES / (1024 * 1024)
                    ),
                };
            }
            WireResponse::Download(WireDownloadResponse { bytes })
        }
        Err(e) => wire_io_err("read", e),
    }
}

fn wire_io_err(op: &str, err: io::Error) -> WireResponse {
    WireResponse::Error {
        kind: format!("{:?}", err.kind()),
        message: format!("{op}: {err}"),
    }
}

/// Constant-time byte comparison. Mirrors
/// `engram-coordinator::api::auth::ct_eq` so a deployment can audit
/// "where do we compare secrets" by grepping the same name across
/// crates. We don't pull `subtle` as a dep — single tight loop,
/// dwarfed by a vsock round trip, and the project's
/// `forbid(unsafe_code)` rules out the SIMD shortcut.
/// Determine the agent's primary IPv4 address by asking the kernel
/// which local IP would be used to reach an off-box destination. The
/// `connect()` call on a UDP socket doesn't actually send a packet —
/// it just sets up the routing decision so `local_addr()` can return
/// the source IP that would be used. Avoids spawning `ip(8)` (which
/// many slim images don't ship) and avoids netlink dependencies.
///
/// Returns `None` if no default route exists (e.g. networking not
/// configured) or if only loopback is available.
fn read_primary_ipv4() -> Option<String> {
    use std::net::{IpAddr, UdpSocket};
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    // Any routable destination works. 192.0.2.1 is RFC5737
    // documentation space — guaranteed unallocated and never
    // actually contacted (UDP connect is route-only).
    sock.connect("192.0.2.1:1").ok()?;
    let local = sock.local_addr().ok()?;
    match local.ip() {
        IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => Some(v4.to_string()),
        _ => None,
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= x ^ y;
    }
    acc == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::WireExecRequest;
    use std::collections::HashMap;
    use tokio::io::duplex;

    /// In-memory `serve_connection` round-trip. We feed a request in
    /// on one end of a duplex pipe, run the handler against the other,
    /// then drain events on the original side.
    async fn run_against(req: WireExecRequest) -> Vec<WireExecEvent> {
        let (mut client, server) = duplex(64 * 1024);

        // Server side: handler reads request, runs cmd, writes events.
        let server_task = tokio::spawn(async move {
            serve_connection(
                server,
                None,
                HarnessSupervisor::new(),
                Arc::new(crate::cacerts::CaCertInstaller::for_tests()),
            )
            .await
        });

        // Client side: wrap the exec request in the multi-verb
        // envelope, then read events until EOF.
        write_msg(&mut client, &WireRequest::Exec(req))
            .await
            .unwrap();
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
        let server_task = tokio::spawn(async move {
            serve_connection(
                server,
                None,
                HarnessSupervisor::new(),
                Arc::new(crate::cacerts::CaCertInstaller::for_tests()),
            )
            .await
        });
        write_msg(
            &mut client,
            &WireRequest::Exec(WireExecRequest {
                command: Vec::new(),
                stdin: None,
                env: HashMap::new(),
                workdir: None,
                timeout_ms: None,
            }),
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
        let server_task = tokio::spawn(async move {
            serve_connection(
                server,
                None,
                HarnessSupervisor::new(),
                Arc::new(crate::cacerts::CaCertInstaller::for_tests()),
            )
            .await
        });
        write_msg(
            &mut client,
            &WireRequest::Exec(WireExecRequest {
                command: vec!["/this/binary/does/not/exist".into()],
                stdin: None,
                env: HashMap::new(),
                workdir: None,
                timeout_ms: None,
            }),
        )
        .await
        .unwrap();
        let err = server_task.await.unwrap().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().contains("spawn"));
    }

    #[tokio::test]
    async fn handshake_with_correct_token_admits_exec_request() {
        // Drive the auth-on path: send WireHandshake first, expect
        // an ok ack, then send WireExecRequest as usual.
        let (mut client, server) = duplex(64 * 1024);
        let token = "shared-secret".to_string();
        let server_task = tokio::spawn(async move {
            serve_connection(
                server,
                Some(token),
                HarnessSupervisor::new(),
                Arc::new(crate::cacerts::CaCertInstaller::for_tests()),
            )
            .await
        });

        write_msg(
            &mut client,
            &WireHandshake {
                token: "shared-secret".into(),
                agent_version: "test".into(),
            },
        )
        .await
        .unwrap();
        let ack: WireHandshakeAck = read_msg(&mut client).await.unwrap();
        assert!(ack.ok, "ack must be ok for matching token; got {ack:?}");

        write_msg(
            &mut client,
            &WireRequest::Exec(WireExecRequest {
                command: vec!["sh".into(), "-c".into(), "printf hi".into()],
                stdin: None,
                env: HashMap::new(),
                workdir: None,
                timeout_ms: None,
            }),
        )
        .await
        .unwrap();

        let mut events = Vec::new();
        while let Ok(ev) = read_msg::<_, WireExecEvent>(&mut client).await {
            let exit = matches!(ev, WireExecEvent::Exit(_));
            events.push(ev);
            if exit {
                break;
            }
        }
        let _ = server_task.await.unwrap();
        let stdout: Vec<u8> = events
            .iter()
            .filter_map(|e| match e {
                WireExecEvent::Stdout(b) => Some(b.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(stdout, b"hi");
    }

    #[tokio::test]
    async fn handshake_with_wrong_token_is_rejected() {
        let (mut client, server) = duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            serve_connection(
                server,
                Some("expected".into()),
                HarnessSupervisor::new(),
                Arc::new(crate::cacerts::CaCertInstaller::for_tests()),
            )
            .await
        });
        write_msg(
            &mut client,
            &WireHandshake {
                token: "wrong".into(),
                agent_version: "test".into(),
            },
        )
        .await
        .unwrap();

        // Agent sends a typed rejection so the host sees a clean
        // reason rather than just a connection drop.
        let ack: WireHandshakeAck = read_msg(&mut client).await.unwrap();
        assert!(!ack.ok);
        assert!(ack.message.is_some());

        let res = server_task.await.unwrap();
        let err = res.expect_err("auth failure must surface as Err");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn auth_disabled_path_skips_handshake_for_back_compat() {
        // expected_token = None means the agent doesn't read or
        // expect a WireHandshake — host can send WireExecRequest
        // straight away. Critical for back-compat with older hosts
        // that don't know about the handshake.
        let (mut client, server) = duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            serve_connection(
                server,
                None,
                HarnessSupervisor::new(),
                Arc::new(crate::cacerts::CaCertInstaller::for_tests()),
            )
            .await
        });
        write_msg(
            &mut client,
            &WireRequest::Exec(WireExecRequest {
                command: vec!["sh".into(), "-c".into(), "printf x".into()],
                stdin: None,
                env: HashMap::new(),
                workdir: None,
                timeout_ms: None,
            }),
        )
        .await
        .unwrap();
        // Drain to exit.
        let mut got_exit = None;
        while let Ok(ev) = read_msg::<_, WireExecEvent>(&mut client).await {
            if let WireExecEvent::Exit(code) = ev {
                got_exit = Some(code);
                break;
            }
        }
        assert_eq!(got_exit, Some(Some(0)));
        let _ = server_task.await.unwrap();
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

    // ---- New-verb tests ------------------------------------------

    /// One-shot helper: send a non-streaming WireRequest and read
    /// back the single WireResponse. Used by the verb tests below.
    async fn round_trip(req: WireRequest) -> WireResponse {
        let (mut client, server) = duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            serve_connection(
                server,
                None,
                HarnessSupervisor::new(),
                Arc::new(crate::cacerts::CaCertInstaller::for_tests()),
            )
            .await
        });
        write_msg(&mut client, &req).await.unwrap();
        let resp: WireResponse = read_msg(&mut client).await.unwrap();
        let _ = server_task.await.unwrap();
        resp
    }

    #[tokio::test]
    async fn ping_returns_pong() {
        let resp = round_trip(WireRequest::Ping).await;
        assert!(matches!(resp, WireResponse::Pong));
    }

    /// StartShell with a fake `ttyd` binary (a tiny shell script that
    /// `exec`s `python3 -m http.server $3` — args from `start_shell`
    /// are `["-W", "-p", "<port>", "<shell>"]`, so $3 is the port).
    /// We pick a random unused port up-front, point ENGRAM_TTYD_BIN
    /// at the script, fire StartShell, and assert the response says
    /// the port is ready. The probe inside agentd does a real TCP
    /// connect to 127.0.0.1:<port> so this confirms the full
    /// "spawn → bind → ready" sequence works.
    ///
    /// Skipped when `python3` isn't on PATH (CI runners without
    /// python). The dev-vm and macOS dev hosts both have it.
    /// Path 0 of start_shell: the port is ALREADY bound by something
    /// else (the bake's init script's pre-started ttyd in prod). We
    /// must NOT try to spawn — that'd fail with EADDRINUSE and the
    /// host's proxy_shell would surface a bogus error. Instead we
    /// recognise the existing listener and return ShellReady with
    /// spawned=false. Validates the fix for the prod failure mode I
    /// observed 2026-05-20 (session accb3924).
    #[tokio::test]
    async fn start_shell_recognises_a_preexisting_listener_without_spawning() {
        // Pick an unused port, then bind our own listener on it
        // BEFORE calling StartShell. agentd should probe, see the
        // listener, and return spawned=false without touching the
        // ENGRAM_TTYD_BIN spawn path.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Keep `listener` alive for the duration of the test. (If we
        // dropped it, the port would close before the probe runs.)
        // Bind blocking accepts in a thread so the kernel actually
        // delivers SYN/ACK during probe_ready.
        let _accept_thread = std::thread::spawn(move || {
            // Accept exactly one connection (the probe). After that
            // we let the listener drop on thread exit.
            if let Ok((_stream, _)) = listener.accept() {
                // probe_ready closes immediately after connecting;
                // we just hold the stream long enough for that to
                // happen, then exit.
            }
        });

        // Make sure ENGRAM_TTYD_BIN points at a nonexistent path so
        // we can confidently assert "no spawn happened" — if the
        // probe path is broken and we fell through to spawn, the
        // test would fail with a spawn-time error.
        let prev = std::env::var("ENGRAM_TTYD_BIN").ok();
        std::env::set_var("ENGRAM_TTYD_BIN", "/nonexistent/ttyd");

        // Ensure no stale handle from a sibling test leaks in.
        let _ = crate::shell::shutdown_for_tests().await;

        let resp = round_trip(WireRequest::StartShell { port: Some(port) }).await;

        if let Some(p) = prev {
            std::env::set_var("ENGRAM_TTYD_BIN", p);
        } else {
            std::env::remove_var("ENGRAM_TTYD_BIN");
        }

        match resp {
            WireResponse::ShellReady {
                port: got_port,
                spawned,
            } => {
                assert_eq!(got_port, port);
                assert!(
                    !spawned,
                    "must NOT spawn when a listener already owns the port (bake's init-script ttyd case)",
                );
            }
            WireResponse::Error { kind, message } => {
                panic!(
                    "should have detected the existing listener instead of erroring: \
                     kind={kind} message={message}",
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }

    #[tokio::test]
    async fn start_shell_spawns_and_probes_a_real_tcp_listener() {
        use std::io::Write;
        use std::time::Duration;
        if std::process::Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| !s.success())
            .unwrap_or(true)
        {
            eprintln!("SKIP: python3 not available; start_shell test relies on it for a fake ttyd");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-ttyd.sh");
        // `$3` is the port (matching start_shell's argv shape).
        std::fs::write(
            &script,
            "#!/bin/sh\nexec python3 -m http.server \"$3\" --bind 127.0.0.1 > /dev/null 2>&1\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        // Pick an unused localhost port by binding briefly then dropping.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        // Scope the env var so we don't pollute sibling tests.
        let prev = std::env::var("ENGRAM_TTYD_BIN").ok();
        std::env::set_var("ENGRAM_TTYD_BIN", &script);

        let resp = round_trip(WireRequest::StartShell { port: Some(port) }).await;

        if let Some(p) = prev {
            std::env::set_var("ENGRAM_TTYD_BIN", p);
        } else {
            std::env::remove_var("ENGRAM_TTYD_BIN");
        }

        match resp {
            WireResponse::ShellReady {
                port: got_port,
                spawned,
            } => {
                assert_eq!(got_port, port);
                assert!(
                    spawned,
                    "first StartShell call should report `spawned = true`"
                );
            }
            WireResponse::Error { kind, message } => {
                panic!("StartShell returned Error: kind={kind} message={message}");
            }
            other => panic!("unexpected response: {other:?}"),
        }

        // Probe directly — agentd already did this internally, but
        // we double-check the listener is genuinely alive after the
        // RPC returned. (Catches any cleanup-on-drop issue if the
        // handler unintentionally drops the Child.)
        std::io::stdout().flush().unwrap();
        let dial = tokio::net::TcpStream::connect(("127.0.0.1", port));
        let dialed = tokio::time::timeout(Duration::from_secs(2), dial).await;
        assert!(
            dialed.is_ok() && dialed.unwrap().is_ok(),
            "post-StartShell TCP connect should succeed",
        );

        // Tear down the fake ttyd so a sibling test running with the
        // same OnceCell starts clean.
        let _ = crate::shell::shutdown_for_tests().await;
    }

    #[tokio::test]
    async fn shutdown_returns_ack() {
        let resp = round_trip(WireRequest::Shutdown).await;
        assert!(matches!(resp, WireResponse::ShutdownAck));
    }

    #[tokio::test]
    async fn stat_existing_file_returns_size_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        std::fs::write(&path, b"hello-stat").unwrap();
        let resp = round_trip(WireRequest::Stat {
            path: path.to_string_lossy().into_owned(),
        })
        .await;
        match resp {
            WireResponse::Stat(s) => {
                assert!(s.exists);
                assert_eq!(s.size, b"hello-stat".len() as u64);
                assert!(!s.is_dir);
                assert!(s.mtime_unix > 0, "mtime should be a real Unix epoch");
            }
            other => panic!("expected Stat response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stat_missing_file_reports_exists_false() {
        let resp = round_trip(WireRequest::Stat {
            path: "/this/path/does/not/exist".into(),
        })
        .await;
        match resp {
            WireResponse::Stat(s) => assert!(!s.exists),
            other => panic!("expected Stat response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stat_directory_reports_is_dir_true() {
        let dir = tempfile::tempdir().unwrap();
        let resp = round_trip(WireRequest::Stat {
            path: dir.path().to_string_lossy().into_owned(),
        })
        .await;
        match resp {
            WireResponse::Stat(s) => {
                assert!(s.exists);
                assert!(s.is_dir);
            }
            other => panic!("expected Stat response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn upload_writes_bytes_creating_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/file.txt");
        let resp = round_trip(WireRequest::Upload {
            path: path.to_string_lossy().into_owned(),
            bytes: b"uploaded-content".to_vec(),
            mode: None,
        })
        .await;
        assert!(matches!(resp, WireResponse::UploadOk));
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk, b"uploaded-content");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn upload_with_mode_chmods_to_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.sh");
        let resp = round_trip(WireRequest::Upload {
            path: path.to_string_lossy().into_owned(),
            bytes: b"#!/bin/sh\necho hi\n".to_vec(),
            mode: Some(0o755),
        })
        .await;
        assert!(matches!(resp, WireResponse::UploadOk));
        let perms = std::fs::metadata(&path).unwrap().permissions();
        // Compare just the user/group/other bits — type bits vary.
        assert_eq!(perms.mode() & 0o777, 0o755);
    }

    #[tokio::test]
    async fn download_returns_file_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        let payload: Vec<u8> = (0..=255u8).collect();
        std::fs::write(&path, &payload).unwrap();
        let resp = round_trip(WireRequest::Download {
            path: path.to_string_lossy().into_owned(),
        })
        .await;
        match resp {
            WireResponse::Download(d) => assert_eq!(d.bytes, payload),
            other => panic!("expected Download response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn download_missing_file_returns_error_response() {
        let resp = round_trip(WireRequest::Download {
            path: "/this/file/does/not/exist".into(),
        })
        .await;
        match resp {
            WireResponse::Error { kind, message } => {
                assert!(
                    kind.contains("NotFound"),
                    "expected NotFound kind, got {kind}"
                );
                assert!(message.contains("read"), "message should name the op");
            }
            other => panic!("expected Error response, got {other:?}"),
        }
    }

    // ---- #567 version-skew NAK ------------------------------------

    /// A guest's agentd is baked into its image at build time, so a live
    /// fleet routinely runs older agentd binaries than the host speaks.
    /// Prod session 8174b7aa hit this: the guest's agentd predated
    /// `WireRequest::StartBrowser`, so the request's variant index
    /// decoded as unknown, `read_msg` failed, and `serve_connection`
    /// returned `Err` having written zero bytes -- indistinguishable
    /// from agentd crashing mid-call. The host only ever saw
    /// `start_browser: recv: early eof`. This test pins that an
    /// undecodable-but-well-framed request instead gets a typed
    /// `WireResponse::Error` naming the skew, before the connection
    /// drops.
    #[tokio::test]
    async fn unknown_request_gets_typed_error_not_eof() {
        let (client, server) = duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            serve_connection(
                server,
                None,
                HarnessSupervisor::new(),
                Arc::new(crate::cacerts::CaCertInstaller::for_tests()),
            )
            .await
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        // Hand-craft a frame whose body is a well-formed length prefix
        // over a bincode enum-variant tag that's out of range for
        // `WireRequest`. `bincode::serialize`/`deserialize` (the free
        // functions `read_msg`/`write_msg` use) default to *fixint*
        // encoding for the free-function API (see
        // `bincode::config` module docs — the `DefaultOptions` struct
        // and the top-level functions disagree on this), so a variant
        // tag is a fixed 4-byte little-endian `u32`. `WireRequest` has
        // well under 9999 variants, so this frame is exactly what an
        // agentd that predates a new variant (or one running a newer
        // protocol than we understand) sees on the wire.
        let bad_variant: u32 = 9999;
        let body = bad_variant.to_le_bytes();
        let len_prefix = (body.len() as u32).to_be_bytes();
        client_writer.write_all(&len_prefix).await.unwrap();
        client_writer.write_all(&body).await.unwrap();

        let resp = tokio::time::timeout(
            Duration::from_secs(5),
            read_msg::<_, WireResponse>(&mut client_reader),
        )
        .await
        .expect(
            "agentd must reply with a typed error instead of silently \
             dropping the connection (#567 version-skew incident)",
        )
        .expect("reply must decode as a WireResponse frame");

        match resp {
            WireResponse::Error { message, .. } => {
                assert!(
                    message.contains(env!("CARGO_PKG_VERSION")),
                    "message should name agentd's version: {message}"
                );
                assert!(
                    message.to_lowercase().contains("skew"),
                    "message should name host/guest version skew: {message}"
                );
                assert!(
                    message.contains("RefreshImage"),
                    "message should name the remedy: {message}"
                );
            }
            other => panic!("expected WireResponse::Error, got {other:?}"),
        }

        let err = server_task.await.unwrap().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    /// Guard rail for the fix above: an ordinary peer disconnect (host
    /// crash, connection reset, or simply closing without sending a
    /// request) must NOT get a NAK written back — there's no
    /// undecodable frame, just an absent one, and the stream may
    /// already be gone.
    #[tokio::test]
    async fn clean_disconnect_gets_no_reply() {
        let (client, server) = duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            serve_connection(
                server,
                None,
                HarnessSupervisor::new(),
                Arc::new(crate::cacerts::CaCertInstaller::for_tests()),
            )
            .await
        });

        let (mut client_reader, mut client_writer) = tokio::io::split(client);
        // Shut down the write half without sending anything -- an
        // ordinary disconnect. (A bare `drop` doesn't work here: the
        // split halves share the underlying `DuplexStream` by
        // reference, so the write direction only actually closes via
        // an explicit `shutdown()`, not by dropping one handle while
        // the other's still alive.)
        client_writer.shutdown().await.unwrap();

        let result = tokio::time::timeout(Duration::from_secs(5), server_task)
            .await
            .expect("serve_connection must return promptly on a clean disconnect")
            .unwrap();
        // Either Ok or Err is acceptable here -- the only thing this
        // test pins is that no reply is written (below).
        let _ = result;

        let mut buf = [0u8; 1];
        let n = tokio::time::timeout(Duration::from_secs(5), client_reader.read(&mut buf))
            .await
            .expect("reading the (absent) reply must not hang")
            .unwrap();
        assert_eq!(
            n, 0,
            "agentd must not write any bytes on a clean disconnect"
        );
    }
}
