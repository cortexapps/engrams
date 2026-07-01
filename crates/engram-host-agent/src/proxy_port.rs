//! ADR 0064 + ADR 0066: host-agent half of the ProxyPort tunnel — the
//! raw-byte, arbitrary-port path that shuttles opaque bytes across a
//! [`PortTunnel`]. No WS framing, no ttyd handshake — the bytes are whatever
//! the inner protocol speaks (HTTP/1.1, h2c, a WebSocket upgrade, gRPC).
//!
//! ADR 0066: the guest hop reaches the dev server on the guest's **`127.0.0.1`**
//! via the in-guest agentd relay ([`open_vsock_tunnel_at`]) — so a server bound
//! to loopback (Vite, the Tilt UI) is reachable, which the old `guest_ip`
//! network dial could not. [`open_tcp_tunnel_at`] survives only for backends
//! without a vsock relay (the Process backend; VZ until its Phase 2 real-vsock
//! migration), dialing `guest_ip:port` directly with no per-VM netns — only FC
//! ever had a netns dial, and FC now always takes the relay.

use std::time::Duration;

use bytes::Bytes;
use engram_core::error::SandboxError;
use engram_core::traits::sandbox::HarnessByteStream;
use engram_core::types::port::PortTunnelEnds;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A dev server is user/agent-managed, so (unlike ttyd) we don't expect
/// a long boot race — but a brief connection-refused retry smooths the
/// "agent just ran `npm run dev`, the preview opened a beat early" case.
/// Deliberately shorter than the shell's 8s ttyd-boot budget: if nothing
/// is listening, fail fast so the orchestrator can surface a clean 502.
const PORT_DIAL_DEADLINE: Duration = Duration::from_secs(3);
const PORT_BACKOFF_START: Duration = Duration::from_millis(100);
const PORT_BACKOFF_MAX: Duration = Duration::from_millis(500);

/// Max bytes per inbound chunk read off the guest socket. Keeps each
/// gRPC `ProxyPortData` message well under tonic's 4 MiB default, so no
/// per-method message-size bump is needed.
const READ_CHUNK: usize = 64 * 1024;

/// ADR 0066: open a raw-byte tunnel to the guest's `127.0.0.1:target_port` via
/// the in-guest agentd relay. `stream` is a fresh vsock connection to the relay
/// listener (from `SandboxBackend::open_guest_stream`); we send the
/// [`RelayConnect`](engram_harness_proto::RelayConnect) header and read the
/// [`RelayAck`](engram_harness_proto::RelayAck) — so a dev server that isn't
/// listening surfaces as a synchronous error (a clean 502), preserving ADR
/// 0064's fail-fast contract — then splice through the unchanged pump. The
/// guest reaches loopback-bound dev servers the host's `guest_ip` dial cannot.
pub async fn open_vsock_tunnel_at(
    mut stream: HarnessByteStream,
    target_port: u16,
    ends: PortTunnelEnds,
) -> Result<(), SandboxError> {
    engram_harness_proto::write_msg(
        &mut stream,
        &engram_harness_proto::RelayConnect { target_port },
    )
    .await
    .map_err(|e| SandboxError::Vm(format!("proxy_port: write relay header: {e}").into()))?;
    let ack: engram_harness_proto::RelayAck = engram_harness_proto::read_msg(&mut stream)
        .await
        .map_err(|e| SandboxError::Vm(format!("proxy_port: read relay ack: {e}").into()))?;
    if !ack.ok {
        return Err(SandboxError::Vm(
            format!(
                "proxy_port: guest relay could not reach 127.0.0.1:{target_port}: {}",
                ack.error.unwrap_or_default()
            )
            .into(),
        ));
    }
    pump_tcp_through_tunnel(stream, ends);
    Ok(())
}

/// Open a raw-byte tunnel by dialing `guest_ip:port` directly (host root netns,
/// no per-VM netns). Used only by backends **without** a vsock relay: the
/// Process backend (`guest_ip` is `127.0.0.1` — agentd is a host subprocess)
/// and VZ until its Phase 2 real-vsock migration (`guest_ip` is the in-VM eth0
/// IP). FC never reaches this path — `open_guest_stream` always hands it the
/// vsock relay — so the old per-VM-netns dial (only FC ever had one) is retired.
pub async fn open_tcp_tunnel_at(
    guest_ip: String,
    port: u16,
    ends: PortTunnelEnds,
) -> Result<(), SandboxError> {
    let stream = connect_cold(&guest_ip, port).await?;
    pump_tcp_through_tunnel(stream, ends);
    Ok(())
}

/// Cold-path dial: raw TCP connect with a short connection-refused
/// retry (same shape as the shell cold path, shorter deadline).
async fn connect_cold(guest_ip: &str, port: u16) -> Result<TcpStream, SandboxError> {
    let addr = format!("{guest_ip}:{port}");
    let deadline = std::time::Instant::now() + PORT_DIAL_DEADLINE;
    let mut backoff = PORT_BACKOFF_START;
    loop {
        match TcpStream::connect(&addr).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                let refused = e.kind() == std::io::ErrorKind::ConnectionRefused;
                if !refused || std::time::Instant::now() >= deadline {
                    return Err(SandboxError::Vm(
                        format!("proxy_port connect {addr}: {e}").into(),
                    ));
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(PORT_BACKOFF_MAX);
            }
        }
    }
}

/// Spawn the bidi pump: `outbound` bytes (caller → guest socket) and
/// `inbound` bytes (guest socket → caller). Returns immediately; the
/// pump runs until either side closes. Mirrors
/// [`crate::proxy_shell::pump_websocket_through_tunnel`] for raw bytes.
/// Generic over the stream so tests can drive it over an in-memory
/// duplex without a real socket.
pub(crate) fn pump_tcp_through_tunnel<S>(stream: S, ends: PortTunnelEnds)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut rd, mut wr) = tokio::io::split(stream);
    let PortTunnelEnds {
        mut outbound_rx,
        inbound_tx,
    } = ends;

    // Outbound: caller → guest socket.
    let outbound = async move {
        while let Some(buf) = outbound_rx.recv().await {
            if wr.write_all(&buf).await.is_err() {
                break;
            }
        }
        // Half-close the write side so the guest sees EOF and can
        // finish its response / tear down cleanly.
        let _ = wr.shutdown().await;
    };

    // Inbound: guest socket → caller.
    let inbound = async move {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) => break, // guest closed (EOF)
                Ok(n) => {
                    if inbound_tx
                        .send(Bytes::copy_from_slice(&buf[..n]))
                        .await
                        .is_err()
                    {
                        break; // caller dropped its receiver
                    }
                }
                Err(_) => break,
            }
        }
    };

    tokio::spawn(async move {
        tokio::select! {
            _ = outbound => {},
            _ = inbound => {},
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::port::PortTunnel;
    use tokio::net::TcpListener;

    /// End-to-end through `open_tcp_tunnel_at`: spin up a localhost TCP
    /// echo server, dial it via the cold path, and confirm raw bytes
    /// round-trip through the [`PortTunnel`]. Doubles as the cold-path
    /// connect regression.
    #[tokio::test]
    async fn open_tcp_tunnel_at_round_trips_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let _server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            // Echo a single read back, then EOF.
            let n = sock.read(&mut buf).await.unwrap();
            sock.write_all(&buf[..n]).await.unwrap();
            let _ = sock.shutdown().await;
        });

        let (tunnel, ends) = PortTunnel::pair();
        open_tcp_tunnel_at(addr.ip().to_string(), addr.port(), ends)
            .await
            .expect("open tunnel");

        let PortTunnel {
            outbound,
            mut inbound,
        } = tunnel;
        outbound
            .send(Bytes::from_static(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"))
            .await
            .unwrap();

        let mut got = Vec::new();
        while let Ok(Some(chunk)) =
            tokio::time::timeout(Duration::from_secs(2), inbound.recv()).await
        {
            got.extend_from_slice(&chunk);
            if got.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        assert!(
            got.starts_with(b"GET / HTTP/1.1"),
            "echoed bytes mismatch: {got:?}"
        );
    }

    /// Connect failure surfaces as an error (no zombie pump) once the
    /// short retry budget elapses. Port 1 on localhost always refuses.
    #[tokio::test]
    async fn open_tcp_tunnel_at_errors_when_server_absent() {
        let (_tunnel, ends) = PortTunnel::pair();
        let err = tokio::time::timeout(
            Duration::from_secs(8),
            open_tcp_tunnel_at("127.0.0.1".into(), 1, ends),
        )
        .await
        .expect("dial timed out test-side")
        .expect_err("connect must surface as error");
        match err {
            SandboxError::Vm(_) => {}
            other => panic!("expected SandboxError::Vm, got {other:?}"),
        }
    }

    /// `open_vsock_tunnel_at`: write the header, read an OK ack from a fake
    /// agentd, then raw bytes round-trip through the `PortTunnel` (the ADR 0066
    /// guest hop, with the vsock stream stubbed by an in-memory duplex).
    #[tokio::test]
    async fn open_vsock_tunnel_at_round_trips_after_ack() {
        let (host_end, mut agentd) = tokio::io::duplex(4096);
        let fake = tokio::spawn(async move {
            let hdr: engram_harness_proto::RelayConnect =
                engram_harness_proto::read_msg(&mut agentd).await.unwrap();
            assert_eq!(hdr.target_port, 3000);
            engram_harness_proto::write_msg(
                &mut agentd,
                &engram_harness_proto::RelayAck {
                    ok: true,
                    error: None,
                },
            )
            .await
            .unwrap();
            let mut buf = vec![0u8; 64];
            let n = agentd.read(&mut buf).await.unwrap();
            agentd.write_all(&buf[..n]).await.unwrap();
            let _ = agentd.shutdown().await;
        });

        let stream: HarnessByteStream = Box::pin(host_end);
        let (tunnel, ends) = PortTunnel::pair();
        open_vsock_tunnel_at(stream, 3000, ends)
            .await
            .expect("open");

        let PortTunnel {
            outbound,
            mut inbound,
        } = tunnel;
        outbound.send(Bytes::from_static(b"ping")).await.unwrap();
        let mut got = Vec::new();
        while let Ok(Some(chunk)) =
            tokio::time::timeout(Duration::from_secs(2), inbound.recv()).await
        {
            got.extend_from_slice(&chunk);
            if got == b"ping" {
                break;
            }
        }
        assert_eq!(got, b"ping");
        let _ = fake.await;
    }

    /// A NAK ack (`ok:false`, dev server unreachable) → `open_vsock_tunnel_at`
    /// errors synchronously (→ clean 502), and never spawns the pump.
    #[tokio::test]
    async fn open_vsock_tunnel_at_errors_on_nak() {
        let (host_end, mut agentd) = tokio::io::duplex(4096);
        let fake = tokio::spawn(async move {
            let _: engram_harness_proto::RelayConnect =
                engram_harness_proto::read_msg(&mut agentd).await.unwrap();
            engram_harness_proto::write_msg(
                &mut agentd,
                &engram_harness_proto::RelayAck {
                    ok: false,
                    error: Some("connection refused".into()),
                },
            )
            .await
            .unwrap();
        });

        let stream: HarnessByteStream = Box::pin(host_end);
        let (_tunnel, ends) = PortTunnel::pair();
        let err = open_vsock_tunnel_at(stream, 3000, ends)
            .await
            .expect_err("nak must surface as error");
        assert!(matches!(err, SandboxError::Vm(_)), "got {err:?}");
        let _ = fake.await;
    }
}
