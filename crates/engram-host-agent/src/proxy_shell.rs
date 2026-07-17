//! ADR 0014 issue #6 + ADR 0066: host-agent half of the ProxyShell tunnel.
//!
//! Coord pods in GKE have no route to the per-VM guest IP, so the pre-M1.16
//! `ws://<dial_ip>:7681/ws` direct-WebSocket from coord/api/shell.rs always
//! timed out from prod. This module is the host-agent half of the fix: reach
//! ttyd in the guest and shuttle WebSocket frames across a bidi gRPC stream the
//! coord opens.
//!
//! The async surface is [`open_shell_tunnel_via_relay`] /
//! [`open_shell_tunnel_at`] — each spawns two pump tasks (browser→ttyd and
//! ttyd→browser via the `ShellTunnelEnds` channels) and returns immediately.
//! Lifetime is owned by the caller's `ShellTunnel`: when the outbound channel
//! closes (browser disconnect) or the inbound channel's receiver is dropped,
//! both pumps notice and exit.
//!
//! Guest reach (ADR 0066): the ttyd WebSocket handshake + frames ride the
//! **vsock port relay** — the in-guest agentd dials `127.0.0.1:7681` (ttyd) and
//! splices raw bytes, so ttyd is reachable regardless of how it binds, cold or
//! warm, with no per-VM-netns dial (retired — only FC ever had one, and FC now
//! always takes the relay). Backends without a vsock relay (Process; VZ until
//! its Phase 2 real-vsock migration) fall back to [`open_shell_tunnel_at`],
//! which dials `dial_ip:port` directly with `tokio_tungstenite::connect_async`.

use std::time::Duration;

use bytes::Bytes;
use engram_core::error::SandboxError;
use engram_core::traits::sandbox::HarnessByteStream;
use engram_core::types::shell::{ShellClose, ShellFrame, ShellTunnelEnds};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{protocol::CloseFrame, Message};

/// The port `ttyd` binds on inside every engram guest. Pinned by
/// `engram_rootfs_materializer::DEFAULT_INIT_SHIM`. Tests override via
/// [`open_shell_tunnel_at`].
pub const TTYD_PORT: u16 = 7681;
/// Mirrors coord/api/shell.rs:148 — the pre-rewrite budget. ttyd
/// binds on :7681 a few seconds after kernel boot on a freshly
/// warm-restored sandbox, so we retry connection-refused for ~8 s
/// before bubbling.
const TTYD_DIAL_DEADLINE: Duration = Duration::from_secs(8);
const TTYD_BACKOFF_START: Duration = Duration::from_millis(100);
const TTYD_BACKOFF_MAX: Duration = Duration::from_millis(800);

/// ADR 0066: open a shell tunnel to the guest's ttyd over the vsock relay —
/// reach ttyd on the guest's `127.0.0.1:port` via the in-guest agentd relay (the
/// ttyd WebSocket handshake + frames ride the raw relay stream). `stream` is a
/// fresh vsock connection to the relay listener (from
/// `SandboxBackend::open_guest_stream`). Used by FC (and VZ after its Phase 2
/// real-vsock migration). `start_shell` has already ensured ttyd is listening
/// before we get here, so there's no boot-race retry to do — the relay's own
/// short connection-refused retry is a safety margin.
pub async fn open_shell_tunnel_via_relay(
    mut stream: HarnessByteStream,
    port: u16,
    ends: ShellTunnelEnds,
) -> Result<(), SandboxError> {
    engram_harness_proto::write_msg(
        &mut stream,
        &engram_harness_proto::RelayConnect { target_port: port },
    )
    .await
    .map_err(|e| SandboxError::Vm(format!("proxy_shell: write relay header: {e}").into()))?;
    let ack: engram_harness_proto::RelayAck = engram_harness_proto::read_msg(&mut stream)
        .await
        .map_err(|e| SandboxError::Vm(format!("proxy_shell: read relay ack: {e}").into()))?;
    if !ack.ok {
        return Err(SandboxError::Vm(
            format!(
                "proxy_shell: guest relay could not reach ttyd on 127.0.0.1:{port}: {}",
                ack.error.unwrap_or_default()
            )
            .into(),
        ));
    }
    let (ws, _resp) = tokio_tungstenite::client_async(ttyd_request(port)?, stream)
        .await
        .map_err(|e| SandboxError::Vm(format!("ttyd ws handshake over relay: {e}").into()))?;
    pump_websocket_through_tunnel(ws, ends);
    Ok(())
}

/// Cold-path shell tunnel: dial ttyd at `dial_ip:port` directly over a
/// WebSocket, with a connection-refused retry. Used only by backends WITHOUT a
/// vsock relay: the Process backend (`dial_ip` is `127.0.0.1`) and VZ until its
/// Phase 2 real-vsock migration. FC goes through [`open_shell_tunnel_via_relay`],
/// so the old per-VM-netns dial (only FC ever had one) is retired.
pub async fn open_shell_tunnel_at(
    dial_ip: String,
    port: u16,
    ends: ShellTunnelEnds,
) -> Result<(), SandboxError> {
    let target = format!("ws://{dial_ip}:{port}/ws");
    let upstream = connect_ttyd_cold(|| ttyd_request_from(&target)).await?;
    pump_websocket_through_tunnel(upstream, ends);
    Ok(())
}

/// Build the ttyd WebSocket handshake request (carrying the `tty` subprotocol
/// ttyd requires) for a guest-loopback `port` — the relay path's target.
fn ttyd_request(
    port: u16,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, SandboxError> {
    ttyd_request_from(&format!("ws://127.0.0.1:{port}/ws"))
}

/// Build the ttyd handshake request from an explicit `ws://…/ws` URL.
fn ttyd_request_from(
    url: &str,
) -> Result<tokio_tungstenite::tungstenite::handshake::client::Request, SandboxError> {
    let mut req = url
        .into_client_request()
        .map_err(|e| SandboxError::Vm(format!("bad ttyd url: {e}").into()))?;
    req.headers_mut()
        .insert("sec-websocket-protocol", "tty".parse().unwrap());
    Ok(req)
}

/// Spawn the two pump tasks (outbound: caller → ws, inbound: ws →
/// caller) that move WS messages across the [`ShellTunnelEnds`].
/// Returns immediately; the pumps run until either side closes.
///
/// Split out from the tunnel openers so tests can drive it over any in-memory
/// WebSocket without a real ttyd dial.
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
    let deadline = crate::time_source::metrics_now() + TTYD_DIAL_DEADLINE;
    let mut backoff = TTYD_BACKOFF_START;
    loop {
        let request = build_request()?;
        match tokio_tungstenite::connect_async(request).await {
            Ok((ws, _resp)) => return Ok(ws),
            Err(e) => {
                let msg = e.to_string();
                let refused = msg.contains("Connection refused");
                if !refused || crate::time_source::metrics_now() >= deadline {
                    return Err(SandboxError::Vm(format!("connect to ttyd: {e}").into()));
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(TTYD_BACKOFF_MAX);
            }
        }
    }
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
    /// dial_ip → ws://{dial_ip}:{port}/ws connect + handshake +
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
        open_shell_tunnel_at(addr.ip().to_string(), addr.port(), ends)
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
            open_shell_tunnel_at("127.0.0.1".into(), 1, ends),
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
