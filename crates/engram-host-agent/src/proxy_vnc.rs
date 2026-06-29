//! ADR 0064: host-agent half of the VNC tunnel. Mirrors `proxy_shell` but the
//! upstream is a RAW TCP connection to the in-guest x11vnc (no WebSocket
//! handshake). RFB bytes ride `ShellFrame::Binary`; the auth-gated relay does
//! websockify's job. Cold path dials `127.0.0.1:5900` directly; warm path reuses
//! `proxy_shell::connect_tcp_in_netns_linux` (setns) for the per-VM netns.
//!
//! The async surface is [`open_vnc_tunnel_at`] — it dials x11vnc, then spawns
//! two raw-byte pump tasks and returns immediately. Lifetime is owned by the
//! caller's `ShellTunnel`: when the outbound channel closes (browser
//! disconnect) or the inbound receiver is dropped (caller gave up), both pumps
//! notice and exit.
use std::time::Duration;

use bytes::Bytes;
use engram_core::error::SandboxError;
use engram_core::types::shell::{ShellFrame, ShellTunnelEnds};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Port x11vnc binds inside the guest (ADR 0064 browser bundle). Matches
/// `engram_agentd::browser::DEFAULT_VNC_PORT`.
pub const VNC_PORT: u16 = 5900;

/// x11vnc binds a few seconds after the browser bundle starts on a freshly
/// warm-restored sandbox, so we retry connection-refused for ~10 s before
/// bubbling — same shape as the shell path's ttyd dial.
const DIAL_DEADLINE: Duration = Duration::from_secs(10);
const BACKOFF_START: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_millis(800);

/// Open a raw-TCP VNC tunnel to the in-guest x11vnc and pump bytes across the
/// [`ShellTunnelEnds`]. Cold (`netns_name == None`) dials directly; warm dials
/// inside the per-VM netns via `setns(2)` (Linux only).
pub async fn open_vnc_tunnel_at(
    guest_ip: String,
    port: u16,
    netns_name: Option<String>,
    ends: ShellTunnelEnds,
) -> Result<(), SandboxError> {
    let stream = match &netns_name {
        None => connect_vnc_cold(&guest_ip, port).await?,
        Some(ns) => connect_vnc_warm(ns, &guest_ip, port).await?,
    };
    pump_tcp_through_tunnel(stream, ends);
    Ok(())
}

/// Cold-path dial: a plain `TcpStream::connect` with a connection-refused
/// retry loop matching the shell path's boot-race tolerance.
async fn connect_vnc_cold(guest_ip: &str, port: u16) -> Result<TcpStream, SandboxError> {
    let deadline = std::time::Instant::now() + DIAL_DEADLINE;
    let mut backoff = BACKOFF_START;
    loop {
        match TcpStream::connect((guest_ip, port)).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                let refused = e.kind() == std::io::ErrorKind::ConnectionRefused;
                if !refused || std::time::Instant::now() >= deadline {
                    return Err(SandboxError::Vm(format!("connect to x11vnc: {e}").into()));
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/// Warm-path dial: reuse the shared netns dialer from `proxy_shell`, just on
/// the x11vnc port. Linux only — non-Linux returns a clean `SandboxError` so
/// the rest of the system surfaces a 503 rather than panicking.
async fn connect_vnc_warm(
    netns_name: &str,
    guest_ip: &str,
    port: u16,
) -> Result<TcpStream, SandboxError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (netns_name, guest_ip, port);
        Err(SandboxError::Vm(
            "proxy_vnc: per-VM netns dial requires Linux (got non-Linux host)".into(),
        ))
    }
    #[cfg(target_os = "linux")]
    {
        let deadline = std::time::Instant::now() + DIAL_DEADLINE;
        let mut backoff = BACKOFF_START;
        loop {
            match crate::proxy_shell::connect_tcp_in_netns_linux(netns_name, guest_ip, port).await {
                Ok(s) => return Ok(s),
                Err(e) => {
                    let refused = format!("{e}").contains("Connection refused");
                    if !refused || std::time::Instant::now() >= deadline {
                        return Err(e);
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                }
            }
        }
    }
}

/// Spawn the two raw-byte pumps. Outbound: caller `ShellFrame::Binary`/`Text`
/// → TCP write. Inbound: TCP read → `ShellFrame::Binary`. VNC upstream has no
/// WebSocket control frames, so `Ping`/`Pong` are no-ops and `Close` breaks.
/// Either side closing tears down both pumps. Returns immediately.
fn pump_tcp_through_tunnel(stream: TcpStream, ends: ShellTunnelEnds) {
    let (mut rd, mut wr) = stream.into_split();
    let ShellTunnelEnds {
        mut outbound_rx,
        inbound_tx,
    } = ends;

    let outbound_pump = async move {
        while let Some(frame) = outbound_rx.recv().await {
            match frame {
                ShellFrame::Binary(b) => {
                    if wr.write_all(&b).await.is_err() {
                        break;
                    }
                }
                ShellFrame::Text(t) => {
                    if wr.write_all(t.as_bytes()).await.is_err() {
                        break;
                    }
                }
                ShellFrame::Close(_) => break,
                // VNC upstream has no WS control frames; ping/pong are no-ops.
                ShellFrame::Ping(_) | ShellFrame::Pong(_) => {}
            }
        }
        let _ = wr.shutdown().await;
    };

    let inbound_pump = async move {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) => break, // x11vnc closed
                Ok(n) => {
                    if inbound_tx
                        .send(ShellFrame::Binary(Bytes::copy_from_slice(&buf[..n])))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "x11vnc read error; closing vnc tunnel");
                    break;
                }
            }
        }
    };

    tokio::spawn(async move {
        tokio::select! {
            _ = outbound_pump => {}
            _ = inbound_pump => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::shell::{ShellFrame, ShellTunnel};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn vnc_tunnel_pumps_raw_bytes_both_ways() {
        // Fake VNC server: echoes bytes back.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 64];
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                sock.write_all(&buf[..n]).await.unwrap();
            }
        });

        let (tunnel, ends) = ShellTunnel::pair();
        open_vnc_tunnel_at("127.0.0.1".into(), port, None, ends)
            .await
            .unwrap();
        let ShellTunnel {
            outbound,
            mut inbound,
        } = tunnel;

        outbound
            .send(ShellFrame::Binary(bytes::Bytes::from_static(
                b"RFB 003.008\n",
            )))
            .await
            .unwrap();
        let got = inbound.recv().await.unwrap();
        match got {
            ShellFrame::Binary(b) => assert_eq!(&b[..], b"RFB 003.008\n"),
            other => panic!("expected Binary, got {other:?}"),
        }
    }
}
