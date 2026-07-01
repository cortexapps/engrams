//! ADR 0066: the in-guest half of the vsock port relay.
//!
//! The host-agent dials this listener ([`PROXY_PORT_VSOCK_PORT`]) once per
//! forwarded browser connection, sends a [`RelayConnect`] naming the guest TCP
//! port, and we dial `127.0.0.1:target_port` *inside* the guest and splice raw
//! bytes. That reaches loopback-bound dev servers (Vite, the Tilt UI, `next
//! dev`) that the host's `guest_ip` dial cannot.
//!
//! **No head-of-line blocking:** one vsock connection per forwarded TCP
//! connection, one task per connection, no shared state on the data path. The
//! single invariant that preserves this — the accept loop spawns *before* the
//! header read + the (retrying) loopback dial, so one slow/refused target port
//! never blocks other forwarded connections.

use std::time::Duration;

use engram_harness_proto::{read_msg, write_msg, RelayAck, RelayConnect, PROXY_PORT_VSOCK_PORT};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

/// Loopback dial budget. A dev server the agent just launched may open a beat
/// late, so smooth a brief connection-refused window; otherwise fail fast so
/// the host surfaces a clean 502. Mirrors the host-agent's old
/// `PORT_DIAL_DEADLINE`, relocated guest-side.
const DIAL_DEADLINE: Duration = Duration::from_secs(3);
const BACKOFF_START: Duration = Duration::from_millis(100);
const BACKOFF_MAX: Duration = Duration::from_millis(500);

/// Per-direction splice buffer. 256 KiB ≥ vsock's ~64 KiB per-connection credit
/// window, so one read drains a full window and one write fills it.
const COPY_BUF: usize = 256 * 1024;

/// Host-protecting backstop on concurrent forwarded connections (defence in
/// depth — the coordinator enforces the real per-session cap of 256, and a
/// guest serves exactly one session, so this is never reached in normal
/// operation). It matches that cap; at two fds per connection (the vsock stream
/// and the loopback dial) it stays well under the default `RLIMIT_NOFILE`, so no
/// raise is needed. Bounds guest fds/tasks if a bug or abusive caller opens
/// connections without limit.
const MAX_INFLIGHT: usize = 256;

/// Bind the relay listener and serve forwarded connections until the transport
/// errors. Best-effort: on a transport without a [`PROXY_PORT_VSOCK_PORT`]
/// device (e.g. an old VZ console bake) `listen` fails and this returns —
/// agentd keeps running and previews fall back to "unavailable" until the image
/// carries this agentd.
pub async fn run_port_relay() {
    let transport = match engram_transport::from_env() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "port relay: no transport; relay disabled");
            return;
        }
    };
    let mut listener = match transport.listen(PROXY_PORT_VSOCK_PORT).await {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(
                error = %e,
                port = PROXY_PORT_VSOCK_PORT,
                "port relay: listen failed; relay disabled",
            );
            return;
        }
    };
    tracing::info!(port = PROXY_PORT_VSOCK_PORT, "port relay listening");

    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT));
    loop {
        match listener.accept().await {
            Ok(stream) => {
                let Ok(permit) = sem.clone().try_acquire_owned() else {
                    // At the backstop; drop this connection (host sees a closed
                    // stream → clean failure) rather than pile up on the guest.
                    tracing::warn!("port relay: in-flight backstop hit; dropping connection");
                    continue;
                };
                // CRITICAL (ADR 0066): spawn BEFORE reading the header or dialing
                // the target. Doing either on the accept path would let one
                // slow/refused port stall every new forwarded connection for up
                // to DIAL_DEADLINE.
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(e) = serve_relay_connection(stream).await {
                        tracing::debug!(error = %e, "port relay connection ended with error");
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "port relay accept failed; relay exiting");
                return;
            }
        }
    }
}

/// Read the [`RelayConnect`] header, dial `127.0.0.1:target_port` (with a short
/// connection-refused retry), reply [`RelayAck`], then splice bytes both ways.
/// Generic over the stream so unit tests drive it over an in-memory duplex.
pub async fn serve_relay_connection<S>(mut stream: S) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let hdr: RelayConnect = read_msg(&mut stream).await?;
    match dial_loopback(hdr.target_port).await {
        Ok(mut loopback) => {
            // Loopback still runs Nagle × delayed-ACK; disable it so small
            // interactive writes (WS control frames, chunked boundaries) don't
            // eat ~40 ms stalls.
            let _ = loopback.set_nodelay(true);
            write_msg(
                &mut stream,
                &RelayAck {
                    ok: true,
                    error: None,
                },
            )
            .await?;
            tokio::io::copy_bidirectional_with_sizes(&mut stream, &mut loopback, COPY_BUF, COPY_BUF)
                .await
                .map(|_| ())
        }
        Err(e) => {
            // Best-effort NAK so the host returns a clean error rather than hang.
            let _ = write_msg(
                &mut stream,
                &RelayAck {
                    ok: false,
                    error: Some(e.to_string()),
                },
            )
            .await;
            Err(e)
        }
    }
}

/// Dial `127.0.0.1:port`, retrying connection-refused within [`DIAL_DEADLINE`].
async fn dial_loopback(port: u16) -> std::io::Result<TcpStream> {
    let deadline = std::time::Instant::now() + DIAL_DEADLINE;
    let mut backoff = BACKOFF_START;
    loop {
        match TcpStream::connect(("127.0.0.1", port)).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                let refused = e.kind() == std::io::ErrorKind::ConnectionRefused;
                if !refused || std::time::Instant::now() >= deadline {
                    return Err(e);
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Header → loopback dial → ack → bytes round-trip. Drives
    /// `serve_relay_connection` over an in-memory duplex (the "host" side) with
    /// a real `127.0.0.1` echo server standing in for the guest dev server.
    #[tokio::test]
    async fn relay_dials_loopback_and_round_trips() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 64];
            let n = s.read(&mut buf).await.unwrap();
            s.write_all(&buf[..n]).await.unwrap();
            let _ = s.shutdown().await;
        });

        let (mut host, guest) = tokio::io::duplex(4096);
        let relay = tokio::spawn(serve_relay_connection(guest));

        write_msg(
            &mut host,
            &RelayConnect {
                target_port: target,
            },
        )
        .await
        .unwrap();
        let ack: RelayAck = read_msg(&mut host).await.unwrap();
        assert!(ack.ok, "ack should be ok: {ack:?}");

        host.write_all(b"ping").await.unwrap();
        let mut got = [0u8; 4];
        host.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"ping");
        // Close the vsock side so the relay's `copy_bidirectional` sees EOF on
        // the guest→loopback direction and returns (otherwise it waits forever
        // for more host bytes and the test hangs).
        drop(host);
        let _ = relay.await;
    }

    /// Nothing listening on the target → NAK (`ok:false`) after the dial
    /// budget, so the host surfaces a clean error (→ 502) rather than a hang.
    /// (Takes ~`DIAL_DEADLINE` since port 1 refuses and we retry.)
    #[tokio::test]
    async fn relay_naks_when_nothing_listening() {
        let (mut host, guest) = tokio::io::duplex(4096);
        let relay = tokio::spawn(serve_relay_connection(guest));

        write_msg(&mut host, &RelayConnect { target_port: 1 })
            .await
            .unwrap();
        let ack: RelayAck = read_msg(&mut host).await.unwrap();
        assert!(!ack.ok, "ack should be a NAK for a refused port");
        assert!(ack.error.is_some());
        let _ = relay.await;
    }
}
