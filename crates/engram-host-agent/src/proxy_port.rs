//! ADR 0064: host-agent half of the ProxyPort tunnel — the raw-byte,
//! arbitrary-port generalization of [`crate::proxy_shell`].
//!
//! Dials a guest TCP `port` (a dev server the agent started) in the
//! right network namespace and shuttles opaque bytes across a
//! [`PortTunnel`]. Cold sandboxes dial from the host root netns; warm-
//! restored sandboxes dial from inside their per-VM netns (Linux only)
//! via [`crate::proxy_shell::connect_tcp_in_netns_linux`]. No WS
//! framing, no ttyd handshake — the bytes are whatever the inner
//! protocol speaks (HTTP/1.1, h2c, a WebSocket upgrade, gRPC).

use std::time::Duration;

use bytes::Bytes;
use engram_core::error::SandboxError;
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

/// Open a raw-byte tunnel from this host to `guest_ip:port`, dialing
/// inside `netns_name` when `Some` (warm-restored sandbox) or on the
/// host root when `None` (cold sandbox). Spawns the bidi pump and
/// returns once connected; the pump lives until either tunnel end
/// closes. Convenience surface mirrored on
/// [`crate::proxy_shell::open_shell_tunnel_at`].
pub async fn open_tcp_tunnel_at(
    guest_ip: String,
    port: u16,
    netns_name: Option<String>,
    ends: PortTunnelEnds,
) -> Result<(), SandboxError> {
    let stream = match &netns_name {
        None => connect_cold(&guest_ip, port).await?,
        Some(ns) => connect_in_netns(ns, &guest_ip, port).await?,
    };
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

/// Warm-path dial: open the TCP socket inside the per-VM netns. Linux
/// only — a non-Linux host can't have a netns sandbox, so a
/// `Some(netns)` there is a clean error (mirrors `proxy_shell`).
async fn connect_in_netns(
    netns_name: &str,
    guest_ip: &str,
    port: u16,
) -> Result<TcpStream, SandboxError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (netns_name, guest_ip, port);
        Err(SandboxError::Vm(
            "proxy_port: per-VM netns dial requires Linux (got non-Linux host)".into(),
        ))
    }
    #[cfg(target_os = "linux")]
    {
        let deadline = std::time::Instant::now() + PORT_DIAL_DEADLINE;
        let mut backoff = PORT_BACKOFF_START;
        loop {
            match crate::proxy_shell::connect_tcp_in_netns_linux(netns_name, guest_ip, port).await {
                Ok(s) => return Ok(s),
                Err(e) => {
                    let refused = format!("{e}").contains("Connection refused");
                    if !refused || std::time::Instant::now() >= deadline {
                        return Err(e);
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(PORT_BACKOFF_MAX);
                }
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
        open_tcp_tunnel_at(addr.ip().to_string(), addr.port(), None, ends)
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
            open_tcp_tunnel_at("127.0.0.1".into(), 1, None, ends),
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
