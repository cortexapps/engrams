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
//!
//! **Supervised, not fire-and-forget (issue #567).** A Firecracker restore
//! re-kicks the vsock device, which can surface an `accept()` error on this
//! listener — anywhere from one transient blip to the listener never coming
//! back. [`run_port_relay_with`] backs off and retries a transient error,
//! [`run_relay_loop`] escalates (gives up on the listener) after too many
//! errors in a row, and the outer supervisor in `run_port_relay_with`
//! re-`listen()`s and keeps going — including containing a panic inside the
//! accept loop as a `JoinError` rather than letting it take down the whole
//! task. One relay-disabled `return` remains, for a transport that has
//! *never* managed to bind 1030 at all (an old image with no vsock device),
//! matching this module's original best-effort posture for that case.

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

/// Backoff on a failed `accept()` (issue #567: a Firecracker restore re-kicks
/// the vsock device, which can surface a transient accept error). Starts
/// short so a one-off blip barely delays the next accept, and doubles up to
/// [`ACCEPT_BACKOFF_MAX`] so a listener that is actually dead doesn't spin
/// hot while we count up to [`MAX_CONSECUTIVE_ACCEPT_ERRORS`].
const ACCEPT_BACKOFF_START: Duration = Duration::from_millis(100);
/// Cap on the accept-error backoff. A restored VM's vsock device comes back
/// within milliseconds once it's back at all, so waiting longer than this
/// between retries would only slow recovery from a real blip.
const ACCEPT_BACKOFF_MAX: Duration = Duration::from_secs(1);
/// How many `accept()` failures in a row before we give up on this listener
/// and return, rather than backing off forever. A transient blip resolves in
/// one or two tries; a listener still failing after this many is not coming
/// back on its own (e.g. the underlying vsock device died outright) — the
/// caller re-listening fresh gets more mileage than an endless retry loop.
const MAX_CONSECUTIVE_ACCEPT_ERRORS: usize = 8;

/// Backoff between re-`listen()` attempts after the *transport* fails to bind
/// 1030 (distinct from `ACCEPT_BACKOFF_*`, which governs a bound listener
/// whose `accept()` calls are failing). Slower to start than the accept
/// backoff since a restore's vsock device may take a beat longer to come
/// back than a single accept blip.
const RESPAWN_BACKOFF_START: Duration = Duration::from_millis(500);
/// Cap on the re-listen backoff: long enough to stop hammering a transport
/// that's genuinely still restoring, short enough that recovery is noticed
/// within a handful of seconds.
const RESPAWN_BACKOFF_MAX: Duration = Duration::from_secs(5);

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

/// Resolve the transport and hand off to [`run_port_relay_with`]. Best-effort:
/// on a guest with no configured transport (e.g. `ENGRAM_TRANSPORT` unset on a
/// dev host with no vsock device) this just returns — agentd keeps running and
/// previews fall back to "unavailable" until the image carries this agentd.
pub async fn run_port_relay() {
    let transport = match engram_transport::from_env() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "port relay: no transport; relay disabled");
            return;
        }
    };
    run_port_relay_with(transport).await;
}

/// Supervise the relay listener for the guest's whole lifetime: bind, serve
/// forwarded connections via [`run_relay_loop`] until it gives up on the
/// listener (persistent accept errors) or panics, then re-`listen()` and go
/// again. Split out of [`run_port_relay`] so unit tests can drive it against
/// a mock [`engram_transport::Transport`].
///
/// One [`Semaphore`](tokio::sync::Semaphore) is created up front and reused
/// across every rebind — the in-flight cap must span rebinds, since
/// connections admitted under a since-dead listener still hold permits until
/// their own tasks finish.
pub(crate) async fn run_port_relay_with(transport: Box<dyn engram_transport::Transport>) {
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT));
    // Distinguishes "never managed to bind at all" (old image / no vsock
    // device — give up quietly, matching pre-#567 behavior) from "bound
    // fine before, this listen() attempt just failed" (a restore's vsock
    // device isn't back yet — worth retrying).
    let mut ever_bound = false;
    let mut respawn_backoff = RESPAWN_BACKOFF_START;
    loop {
        let listener = match transport.listen(PROXY_PORT_VSOCK_PORT).await {
            Ok(l) => l,
            Err(e) if !ever_bound => {
                tracing::warn!(
                    error = %e,
                    port = PROXY_PORT_VSOCK_PORT,
                    "port relay: listen failed; relay disabled",
                );
                return;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    port = PROXY_PORT_VSOCK_PORT,
                    backoff_ms = respawn_backoff.as_millis(),
                    "port relay: re-listen failed; retrying",
                );
                tokio::time::sleep(respawn_backoff).await;
                respawn_backoff = (respawn_backoff * 2).min(RESPAWN_BACKOFF_MAX);
                continue;
            }
        };
        ever_bound = true;
        respawn_backoff = RESPAWN_BACKOFF_START;
        tracing::info!(port = PROXY_PORT_VSOCK_PORT, "port relay listening");

        // Run the accept loop on its own task and await the JoinHandle
        // (instead of `.await`ing run_relay_loop directly) so a panic inside
        // it surfaces here as a JoinError rather than unwinding into — and
        // killing — this supervisor (and whatever spawned `run_port_relay`).
        match tokio::spawn(run_relay_loop(listener, sem.clone())).await {
            Ok(()) => tracing::warn!("port relay: accept loop exited; re-listening"),
            Err(join_err) => tracing::warn!(
                error = %join_err,
                "port relay: accept loop panicked; re-listening",
            ),
        }
    }
}

/// The accept loop proper: pull connections off `listener` and hand each to
/// its own `serve_relay_connection` task until told to stop. Split out of
/// [`run_port_relay_with`] so unit tests can drive it directly over a
/// scripted [`engram_transport::Listener`] instead of a real transport.
async fn run_relay_loop(
    mut listener: Box<dyn engram_transport::Listener>,
    sem: std::sync::Arc<tokio::sync::Semaphore>,
) {
    let mut backoff = ACCEPT_BACKOFF_START;
    let mut consecutive_errors = 0usize;
    loop {
        match listener.accept().await {
            Ok(stream) => {
                // A live accept means the listener is healthy again; forget
                // any backoff/error streak we'd built up from prior errors.
                backoff = ACCEPT_BACKOFF_START;
                consecutive_errors = 0;
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
                // Issue #567: don't die on the first accept error. A restore's
                // vsock re-kick can surface exactly one transient failure here;
                // log and keep going with a short, doubling backoff rather than
                // taking down Shell + Browser for the VM's remaining life.
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ACCEPT_ERRORS {
                    tracing::warn!(
                        error = %e,
                        consecutive_errors,
                        "port relay: listener looks dead; exiting for a rebind",
                    );
                    return;
                }
                tracing::warn!(
                    error = %e,
                    consecutive_errors,
                    backoff_ms = backoff.as_millis(),
                    "port relay accept failed; backing off and retrying",
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(ACCEPT_BACKOFF_MAX);
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
    use engram_transport::Listener;
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

    /// Test double for [`engram_transport::Listener`], driven by a queue of
    /// scripted `accept()` outcomes. Once the queue drains, `accept()` pends
    /// forever rather than erroring or panicking — it stands in for a
    /// listener that has simply gone quiet, so tests can assert "nothing
    /// further happens" by racing the loop against a timeout instead of a
    /// synthetic terminal error.
    struct ScriptedListener {
        script: std::collections::VecDeque<std::io::Result<engram_transport::BoxedStream>>,
    }

    #[async_trait::async_trait]
    impl engram_transport::Listener for ScriptedListener {
        async fn accept(&mut self) -> std::io::Result<engram_transport::BoxedStream> {
            match self.script.pop_front() {
                Some(outcome) => outcome,
                None => std::future::pending().await,
            }
        }
    }

    /// Build a scripted "successful accept": the guest half of an in-memory
    /// duplex, boxed the way a real `Listener::accept()` hands a stream to
    /// `run_relay_loop`, plus the host half the test drives directly.
    fn scripted_connection() -> (tokio::io::DuplexStream, engram_transport::BoxedStream) {
        let (host, guest) = tokio::io::duplex(4096);
        (host, Box::pin(guest))
    }

    /// Drive one RelayConnect → ack → echo round trip over `host`, against a
    /// throwaway `127.0.0.1` echo server standing in for the guest dev
    /// server. Factored out of `relay_dials_loopback_and_round_trips` above
    /// so the backoff/supervisor tests below can reuse the same handshake
    /// without retyping it.
    async fn assert_round_trip_succeeds(host: &mut tokio::io::DuplexStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 64];
            let n = s.read(&mut buf).await.unwrap();
            s.write_all(&buf[..n]).await.unwrap();
            let _ = s.shutdown().await;
        });

        write_msg(
            host,
            &RelayConnect {
                target_port: target,
            },
        )
        .await
        .unwrap();
        let ack: RelayAck = read_msg(host).await.unwrap();
        assert!(ack.ok, "ack should be ok: {ack:?}");

        host.write_all(b"ping").await.unwrap();
        let mut got = [0u8; 4];
        host.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"ping");
    }

    /// ADR 0066 / issue #567: a Firecracker restore re-kicks the vsock
    /// device, which can surface exactly one transient `accept()` error. The
    /// loop must log it and keep accepting rather than exiting for good —
    /// today's (buggy) code returns immediately, killing Shell + Browser for
    /// the VM's remaining life.
    #[tokio::test]
    async fn relay_survives_transient_accept_error() {
        let (mut host, guest) = scripted_connection();
        let listener = ScriptedListener {
            script: std::collections::VecDeque::from([
                Err(std::io::Error::other("transient accept blip")),
                Ok(guest),
            ]),
        };
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT));
        let loop_task = tokio::spawn(run_relay_loop(Box::new(listener), sem));

        tokio::time::timeout(
            Duration::from_secs(5),
            assert_round_trip_succeeds(&mut host),
        )
        .await
        .expect("round trip should succeed despite the earlier transient accept error");

        loop_task.abort();
    }

    /// A listener that always fails is not a transient blip — it's dead
    /// (e.g. the vsock device never came back after a restore). Backing off
    /// forever would leave the relay silently wedged; the loop must give up
    /// after enough consecutive failures so its caller can re-listen fresh.
    #[tokio::test(start_paused = true)]
    async fn relay_exits_after_persistent_accept_errors() {
        let listener = ScriptedListener {
            script: std::iter::repeat_with(|| Err(std::io::Error::other("listener is dead")))
                .take(64)
                .collect(),
        };
        let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT));

        // Paused time makes every backoff sleep resolve instantly, so this
        // is bounded by iteration count, not wall clock — a generous
        // timeout just guards against an infinite loop hanging the test.
        tokio::time::timeout(
            Duration::from_secs(30),
            run_relay_loop(Box::new(listener), sem),
        )
        .await
        .expect("run_relay_loop should return once the listener looks permanently dead");
    }

    /// Mock [`engram_transport::Transport`] for the supervisor test:
    /// `listen()` pops one scripted outcome per rebind attempt off a queue.
    /// `dial()` is never called by the port relay (that's the harness/forge
    /// bridges' job), so it's a stub.
    struct MockTransport {
        listeners: std::sync::Mutex<std::collections::VecDeque<std::io::Result<Box<dyn Listener>>>>,
    }

    #[async_trait::async_trait]
    impl engram_transport::Transport for MockTransport {
        async fn dial(&self, _port: u32) -> std::io::Result<engram_transport::BoxedStream> {
            unreachable!("port relay never dials out; only listen() is under test")
        }

        async fn listen(&self, _port: u32) -> std::io::Result<Box<dyn Listener>> {
            self.listeners
                .lock()
                .unwrap()
                .pop_front()
                .expect("listen() called more times than the test scripted")
        }
    }

    /// ADR 0066 / issue #567: a dead listener (the escalation case above, or
    /// a restore whose vsock device never comes back) must not be the end of
    /// the relay's life — the supervisor should re-listen and keep serving.
    /// `Transport::listen` hands back a listener that dies immediately (call
    /// 1), then a live one (call 2); the relay must recover onto it.
    #[tokio::test]
    async fn supervisor_rebinds_after_listener_death() {
        let dead_listener: Box<dyn Listener> = Box::new(ScriptedListener {
            script: std::iter::repeat_with(|| Err(std::io::Error::other("listener is dead")))
                .take(MAX_CONSECUTIVE_ACCEPT_ERRORS)
                .collect(),
        });
        let (mut host, guest) = scripted_connection();
        let live_listener: Box<dyn Listener> = Box::new(ScriptedListener {
            script: std::collections::VecDeque::from([Ok(guest)]),
        });

        let transport = MockTransport {
            listeners: std::sync::Mutex::new(std::collections::VecDeque::from([
                Ok(dead_listener),
                Ok(live_listener),
            ])),
        };

        let supervisor = tokio::spawn(run_port_relay_with(Box::new(transport)));

        // Generous: the dead listener burns ~7 real backoff sleeps
        // (100ms..1s capped) before escalating, then the supervisor
        // re-listens onto the live one with no further delay.
        tokio::time::timeout(
            Duration::from_secs(15),
            assert_round_trip_succeeds(&mut host),
        )
        .await
        .expect("round trip should succeed once the supervisor rebinds onto the live listener");

        supervisor.abort();
    }
}
