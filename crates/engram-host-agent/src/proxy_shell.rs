//! ADR 0014 issue #6: host-agent half of the ProxyShell tunnel.
//!
//! Coord pods in GKE have no route to the per-VM `guest_ip`
//! (10.200.0.x lives behind a TAP on the FC host VM, or behind a
//! per-VM netns for warm-restored sandboxes), so the pre-M1.16
//! `ws://<guest_ip>:7681/ws` direct-WebSocket from coord/api/shell.rs
//! always times out from prod. This module is the host-agent half of
//! the fix: dial ttyd in the right network namespace and shuttle
//! WebSocket frames across a bidi gRPC stream the coord opens.
//!
//! The async surface is `open_shell_tunnel` — it spawns two pump
//! tasks (browser→ttyd and ttyd→browser via the `ShellTunnelEnds`
//! channels) and returns immediately. Lifetime is owned by the
//! caller's `ShellTunnel`: when the outbound channel closes (browser
//! disconnect) or the inbound channel's receiver is dropped (caller
//! gave up), both pumps notice and exit.
//!
//! Netns entry strategy:
//!
//! - **Cold path (`netns_name == None`)**: connect via
//!   `tokio_tungstenite::connect_async` directly. Same wire shape
//!   the pre-M1.16 coord used; the host can reach the VM's `/30`
//!   from its root netns.
//! - **Warm path (`netns_name == Some(_)`, Linux only)**: open a TCP
//!   socket from inside the target netns via `setns(CLONE_NEWNET)`
//!   on a dedicated worker thread (Tokio's blocking pool), restore
//!   root netns, then hand the connected `TcpStream` back to async
//!   land and run the WebSocket handshake over it. The kernel only
//!   honours the calling thread's netns at `socket(2)` time — once
//!   the FD exists it has its netns burned in.

use std::time::Duration;

use bytes::Bytes;
use engram_core::error::SandboxError;
use engram_core::types::shell::{ShellClose, ShellFrame, ShellTunnelEnds};
use futures::{SinkExt, StreamExt};
#[cfg(target_os = "linux")]
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{protocol::CloseFrame, Message};

/// The port `ttyd` binds on inside every engram guest. Pinned by
/// `engram-image-builder::DEFAULT_INIT_SHIM`. Tests override via
/// [`open_shell_tunnel_at`].
pub const TTYD_PORT: u16 = 7681;
/// Mirrors coord/api/shell.rs:148 — the pre-rewrite budget. ttyd
/// binds on :7681 a few seconds after kernel boot on a freshly
/// warm-restored sandbox, so we retry connection-refused for ~8 s
/// before bubbling.
const TTYD_DIAL_DEADLINE: Duration = Duration::from_secs(8);
const TTYD_BACKOFF_START: Duration = Duration::from_millis(100);
const TTYD_BACKOFF_MAX: Duration = Duration::from_millis(800);

/// Open a shell tunnel from this host to the in-guest `ttyd` for
/// `guest_ip`, running the WebSocket dial inside `netns_name` if
/// `Some` (warm-restored sandbox) or on the host root if `None`
/// (cold sandbox). Convenience wrapper around [`open_shell_tunnel_at`]
/// that pins the port to [`TTYD_PORT`] — production callers want
/// that; tests parameterize it.
pub async fn open_shell_tunnel(
    guest_ip: String,
    netns_name: Option<String>,
    ends: ShellTunnelEnds,
) -> Result<(), SandboxError> {
    open_shell_tunnel_at(guest_ip, TTYD_PORT, netns_name, ends).await
}

/// Same as [`open_shell_tunnel`] but takes an explicit port. Used by
/// the test harness so it can hit a fake ttyd on an arbitrary
/// localhost port; production callers go through `open_shell_tunnel`
/// with the pinned [`TTYD_PORT`].
pub async fn open_shell_tunnel_at(
    guest_ip: String,
    port: u16,
    netns_name: Option<String>,
    ends: ShellTunnelEnds,
) -> Result<(), SandboxError> {
    let target = format!("ws://{guest_ip}:{port}/ws");
    let request = || {
        let mut req = target
            .as_str()
            .into_client_request()
            .map_err(|e| SandboxError::Vm(format!("bad ttyd url: {e}").into()))?;
        req.headers_mut()
            .insert("sec-websocket-protocol", "tty".parse().unwrap());
        Ok::<_, SandboxError>(req)
    };

    let upstream = match &netns_name {
        None => connect_ttyd_cold(request).await?,
        Some(ns) => connect_ttyd_in_netns(ns, &guest_ip, request).await?,
    };

    pump_websocket_through_tunnel(upstream, ends);
    Ok(())
}

/// Spawn the two pump tasks (outbound: caller → ws, inbound: ws →
/// caller) that move WS messages across the [`ShellTunnelEnds`].
/// Returns immediately; the pumps run until either side closes.
///
/// Extracted from [`open_shell_tunnel`] so tests can drive it over
/// any in-memory WebSocket without going through the URL formatter
/// (which hardcodes ttyd's :7681).
pub(crate) fn pump_websocket_through_tunnel<S>(
    upstream: tokio_tungstenite::WebSocketStream<S>,
    ends: ShellTunnelEnds,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut up_sink, mut up_stream) = upstream.split();
    let ShellTunnelEnds {
        mut outbound_rx,
        inbound_tx,
    } = ends;

    // Outbound: caller → ttyd. Caller pushes ShellFrame; we
    // translate to tungstenite Message and forward.
    let outbound_pump = async move {
        while let Some(frame) = outbound_rx.recv().await {
            let msg = match frame {
                ShellFrame::Text(t) => Message::Text(t),
                ShellFrame::Binary(b) => Message::Binary(b.to_vec()),
                ShellFrame::Ping(b) => Message::Ping(b.to_vec()),
                ShellFrame::Pong(b) => Message::Pong(b.to_vec()),
                ShellFrame::Close(cf) => Message::Close(cf.map(|c| CloseFrame {
                    code: c.code.into(),
                    reason: c.reason.into(),
                })),
            };
            if up_sink.send(msg).await.is_err() {
                break;
            }
        }
        // Flush a clean close upstream so ttyd tears down its PTY
        // child rather than leaving zombie processes.
        let _ = up_sink.close().await;
    };

    // Inbound: ttyd → caller. Translate tungstenite Message →
    // ShellFrame.
    let inbound_pump = async move {
        while let Some(msg) = up_stream.next().await {
            let frame = match msg {
                Ok(Message::Text(t)) => ShellFrame::Text(t),
                Ok(Message::Binary(b)) => ShellFrame::Binary(Bytes::from(b)),
                Ok(Message::Ping(b)) => ShellFrame::Ping(Bytes::from(b)),
                Ok(Message::Pong(b)) => ShellFrame::Pong(Bytes::from(b)),
                Ok(Message::Close(Some(cf))) => ShellFrame::Close(Some(ShellClose {
                    code: cf.code.into(),
                    reason: cf.reason.to_string(),
                })),
                Ok(Message::Close(None)) => ShellFrame::Close(None),
                // Tungstenite emits Frame for raw frames in
                // non-default code paths; the standard WS path
                // never produces these, so just drop.
                Ok(Message::Frame(_)) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, "ttyd recv error; closing tunnel");
                    break;
                }
            };
            if inbound_tx.send(frame).await.is_err() {
                break;
            }
        }
    };

    tokio::spawn(async move {
        tokio::select! {
            _ = outbound_pump => {},
            _ = inbound_pump => {},
        }
    });
}

/// Cold-path dial: standard tokio_tungstenite::connect_async with
/// a connection-refused retry loop matching the pre-rewrite coord
/// behaviour (ttyd may need a few seconds after kernel boot before
/// it binds :7681).
async fn connect_ttyd_cold(
    build_request: impl Fn() -> Result<
        tokio_tungstenite::tungstenite::handshake::client::Request,
        SandboxError,
    >,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    SandboxError,
> {
    let deadline = std::time::Instant::now() + TTYD_DIAL_DEADLINE;
    let mut backoff = TTYD_BACKOFF_START;
    loop {
        let request = build_request()?;
        match tokio_tungstenite::connect_async(request).await {
            Ok((ws, _resp)) => return Ok(ws),
            Err(e) => {
                let msg = e.to_string();
                let refused = msg.contains("Connection refused");
                if !refused || std::time::Instant::now() >= deadline {
                    return Err(SandboxError::Vm(format!("connect to ttyd: {e}").into()));
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(TTYD_BACKOFF_MAX);
            }
        }
    }
}

/// Warm-path dial: open a TCP socket inside `netns_name` via
/// `setns(2)`, then run the WebSocket handshake over the resulting
/// `tokio::net::TcpStream`. Linux only — non-Linux returns a clean
/// SandboxError so the rest of the system surfaces a 503 rather
/// than panicking.
async fn connect_ttyd_in_netns(
    netns_name: &str,
    guest_ip: &str,
    build_request: impl Fn() -> Result<
        tokio_tungstenite::tungstenite::handshake::client::Request,
        SandboxError,
    >,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    SandboxError,
> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (netns_name, guest_ip, build_request);
        Err(SandboxError::Vm(
            "proxy_shell: per-VM netns dial requires Linux (got non-Linux host)".into(),
        ))
    }
    #[cfg(target_os = "linux")]
    {
        // ADR 0014 issue #6 follow-up: retry connection-refused for
        // up to TTYD_DIAL_DEADLINE, same shape as the cold path. ttyd
        // inside the warm-restored guest takes a few seconds to bind
        // :7681 — the same boot-race the cold path retries against.
        // Without this, the dashboard's first SHELL-tab click after
        // a fresh warm-lease races ttyd's bind and the user sees
        // "abnormal close" (observed on session 9d9fef3e, 2026-05-20).
        let deadline = std::time::Instant::now() + TTYD_DIAL_DEADLINE;
        let mut backoff = TTYD_BACKOFF_START;
        loop {
            let stream = match connect_tcp_in_netns_linux(netns_name, guest_ip).await {
                Ok(s) => s,
                Err(e) => {
                    let msg = format!("{e}");
                    let refused = msg.contains("Connection refused");
                    if !refused || std::time::Instant::now() >= deadline {
                        return Err(e);
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(TTYD_BACKOFF_MAX);
                    continue;
                }
            };
            // TCP connect succeeded — now do the WS handshake. If the
            // handshake itself fails it's a different problem (ttyd
            // emitted a non-WS reply, etc.) and retrying connect-refused
            // wouldn't help, so we just surface the error.
            //
            // Wrap as MaybeTlsStream::Plain so the WS handshake
            // produces a `WebSocketStream<MaybeTlsStream<TcpStream>>`
            // matching the cold path's return type exactly.
            let wrapped = tokio_tungstenite::MaybeTlsStream::Plain(stream);
            let request = build_request()?;
            let (ws, _resp) = tokio_tungstenite::client_async(request, wrapped)
                .await
                .map_err(|e| SandboxError::Vm(format!("ttyd ws handshake: {e}").into()))?;
            return Ok(ws);
        }
    }
}

#[cfg(target_os = "linux")]
async fn connect_tcp_in_netns_linux(
    netns_name: &str,
    guest_ip: &str,
) -> Result<TcpStream, SandboxError> {
    let netns_path = format!("/var/run/netns/{netns_name}");
    let guest_addr = format!("{guest_ip}:{TTYD_PORT}");

    // We need to:
    //   1. open /proc/self/ns/net to remember root netns.
    //   2. open `netns_path` to get the target ns fd.
    //   3. setns(target) on a dedicated worker thread.
    //   4. std::net::TcpStream::connect inside target.
    //   5. setns(root) to restore.
    //   6. hand the FD back to tokio.
    //
    // Steps 1-5 all need to run on the same thread (setns affects
    // the calling thread's netns). Step 6 happens on the tokio
    // runtime after spawn_blocking returns.

    let netns_path_clone = netns_path.clone();
    let guest_addr_clone = guest_addr.clone();
    let std_stream = tokio::task::spawn_blocking(move || -> Result<std::net::TcpStream, String> {
        use nix::sched::{setns, CloneFlags};
        let root = std::fs::File::open("/proc/self/ns/net")
            .map_err(|e| format!("open /proc/self/ns/net: {e}"))?;
        let target = std::fs::File::open(&netns_path_clone)
            .map_err(|e| format!("open {netns_path_clone}: {e}"))?;
        // nix 0.31 takes anything that implements AsFd — passing
        // `&File` works (File implements AsFd) and keeps both fds
        // alive across the restore call.
        setns(&target, CloneFlags::CLONE_NEWNET).map_err(|e| format!("setns(target): {e}"))?;
        let stream_result = std::net::TcpStream::connect(&guest_addr_clone)
            .map_err(|e| format!("connect {guest_addr_clone}: {e}"));
        // Always restore root netns, even on connect failure — the
        // blocking worker thread is reused and we mustn't leave it
        // pinned to a guest netns.
        let restore =
            setns(&root, CloneFlags::CLONE_NEWNET).map_err(|e| format!("setns(root): {e}"));
        let stream = stream_result?;
        restore?;
        stream.set_nonblocking(true).map_err(|e| e.to_string())?;
        Ok(stream)
    })
    .await
    .map_err(|e| SandboxError::Vm(format!("netns connect worker panicked: {e}").into()))?
    .map_err(|e| SandboxError::Vm(format!("netns connect: {e}").into()))?;

    TcpStream::from_std(std_stream)
        .map_err(|e| SandboxError::Vm(format!("adopt netns TcpStream into tokio: {e}").into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use engram_core::types::shell::{ShellFrame, ShellTunnel, SHELL_TUNNEL_CHANNEL_CAPACITY};
    use tokio::net::TcpListener;

    /// End-to-end pump test: spin up a fake ttyd that does the
    /// server-side WS handshake on a localhost port, dial it from
    /// the client side, attach the pump tasks, and verify a binary
    /// frame round-trips through the [`ShellTunnel`]. Covers the
    /// frame translation in both directions; the netns dial and the
    /// URL formatter are tested separately by integration tests.
    #[tokio::test]
    async fn pump_round_trips_binary_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();
            use futures::stream::StreamExt;
            use tokio_tungstenite::tungstenite::Message;
            if let Some(Ok(msg)) = ws.next().await {
                let bytes = match msg {
                    Message::Binary(b) => b,
                    Message::Text(t) => t.into_bytes(),
                    _ => Vec::new(),
                };
                use futures::SinkExt;
                ws.send(Message::Binary(bytes)).await.unwrap();
                let _ = ws.close(None).await;
            }
        });

        let req = format!("ws://{addr}/").into_client_request().unwrap();
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();

        let (tunnel, ends) = ShellTunnel::pair();
        pump_websocket_through_tunnel(ws, ends);

        let ShellTunnel {
            outbound,
            mut inbound,
        } = tunnel;
        outbound
            .send(ShellFrame::Binary(Bytes::from_static(b"hello-shell")))
            .await
            .unwrap();
        let echoed = tokio::time::timeout(Duration::from_secs(2), inbound.recv())
            .await
            .expect("inbound recv timed out")
            .expect("tunnel closed without echo");
        match echoed {
            ShellFrame::Binary(b) => assert_eq!(&b[..], b"hello-shell"),
            other => panic!("expected Binary echo, got {other:?}"),
        }
    }

    /// Text + ping/pong + close frames also round-trip cleanly
    /// (regression for the translation table in
    /// `pump_websocket_through_tunnel`).
    #[tokio::test]
    async fn pump_round_trips_text_and_close() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();
            use futures::stream::StreamExt;
            use tokio_tungstenite::tungstenite::Message;
            // Echo until the client sends Close, then close back.
            while let Some(Ok(msg)) = ws.next().await {
                use futures::SinkExt;
                match msg {
                    Message::Text(t) => {
                        ws.send(Message::Text(format!("echo:{t}"))).await.unwrap();
                    }
                    Message::Close(_) => {
                        let _ = ws.close(None).await;
                        break;
                    }
                    _ => {}
                }
            }
        });

        let req = format!("ws://{addr}/").into_client_request().unwrap();
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();

        let (tunnel, ends) = ShellTunnel::pair();
        pump_websocket_through_tunnel(ws, ends);

        let ShellTunnel {
            outbound,
            mut inbound,
        } = tunnel;
        outbound
            .send(ShellFrame::Text("ping".into()))
            .await
            .unwrap();
        let echoed = tokio::time::timeout(Duration::from_secs(2), inbound.recv())
            .await
            .unwrap()
            .unwrap();
        match echoed {
            ShellFrame::Text(t) => assert_eq!(t, "echo:ping"),
            other => panic!("expected Text echo, got {other:?}"),
        }
        // Send a close — server echoes close and the inbound channel
        // drains then ends.
        outbound.send(ShellFrame::Close(None)).await.unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(2), inbound.recv()).await {
                Ok(Some(ShellFrame::Close(_))) | Ok(None) => break,
                Ok(Some(other)) => {
                    tracing::debug!("ignoring trailing frame: {other:?}");
                }
                Err(_) => panic!("inbound never closed after CLOSE round-trip"),
            }
        }
    }

    /// SHELL_TUNNEL_CHANNEL_CAPACITY must be a sensible positive
    /// number; the channels constructed in `ShellTunnel::pair` would
    /// panic if zero. Evaluated at compile time so a const-zero
    /// regression breaks the build rather than just the test.
    const _: () = {
        assert!(SHELL_TUNNEL_CHANNEL_CAPACITY > 0);
    };

    /// End-to-end via `open_shell_tunnel_at` — covers the full
    /// guest_ip → ws://{guest_ip}:{port}/ws connect + handshake +
    /// pump path. Doubles as the cold-path regression for the
    /// connection-refused retry.
    #[tokio::test]
    async fn open_shell_tunnel_at_connects_and_round_trips() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();
            use futures::SinkExt;
            use futures::StreamExt;
            use tokio_tungstenite::tungstenite::Message;
            if let Some(Ok(Message::Binary(b))) = ws.next().await {
                ws.send(Message::Binary(b)).await.unwrap();
                let _ = ws.close(None).await;
            }
        });

        let (tunnel, ends) = ShellTunnel::pair();
        open_shell_tunnel_at(addr.ip().to_string(), addr.port(), None, ends)
            .await
            .expect("open tunnel");

        let ShellTunnel {
            outbound,
            mut inbound,
        } = tunnel;
        outbound
            .send(ShellFrame::Binary(Bytes::from_static(b"e2e")))
            .await
            .unwrap();
        let echoed = tokio::time::timeout(Duration::from_secs(2), inbound.recv())
            .await
            .unwrap()
            .unwrap();
        match echoed {
            ShellFrame::Binary(b) => assert_eq!(&b[..], b"e2e"),
            other => panic!("expected Binary echo, got {other:?}"),
        }
    }

    /// Connect failure surfaces synchronously (no zombie pumps).
    #[tokio::test]
    async fn open_shell_tunnel_at_errors_when_server_absent() {
        let (_tunnel, ends) = ShellTunnel::pair();
        // 127.0.0.1:1 is reserved + always refuses. The default 8s
        // retry budget triggers on connection-refused — we want to
        // confirm we eventually bail, but the test needs to be fast,
        // so we patch around it: use port 1 with a localhost target,
        // and tolerate up to TTYD_DIAL_DEADLINE here.
        let err = tokio::time::timeout(
            Duration::from_secs(12),
            open_shell_tunnel_at("127.0.0.1".into(), 1, None, ends),
        )
        .await
        .expect("dial timed out test-side")
        .expect_err("connect must surface as error");
        match err {
            SandboxError::Vm(_) => {}
            other => panic!("expected SandboxError::Vm, got {other:?}"),
        }
    }
}
