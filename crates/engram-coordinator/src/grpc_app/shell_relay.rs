//! `ShellRelayService` over gRPC (ADR 0039 §8, Task 12).
//!
//! The orchestrator uses this bidi-streaming RPC to bridge a terminal
//! session through the coordinator into the session's `ttyd` process.
//! The first inbound frame **must** be `open { session_id }` — this
//! replaces the WebSocket path parameter that the axum shell handler
//! reads from the URL.  Subsequent frames map 1:1 to `ShellFrame`.
//!
//! The session-lifecycle contract is identical to the WS handler
//! (`api/shell.rs`): `ensure_active` auto-resumes Idle/Evacuating
//! sessions; `acquire_shell` non-fatally pins the sandbox against
//! idle eviction; `proxy_shell` opens the host-agent tunnel;
//! `release_shell` is called on every exit path.
//!
//! ## Teardown under tonic
//!
//! The WS handler (`api/shell.rs`) calls `release_shell` synchronously
//! after its bridge future resolves.  Tonic *drops* the server-side
//! future when the client disconnects, which means `release_shell`
//! cannot be `await`ed inside the `Drop` of a guard.  We solve this
//! with a RAII guard whose `Drop` impl spawns a detached task that
//! performs the async release.  On the normal (non-cancel) exit path
//! the explicit `release_shell` call is made before the guard goes out
//! of scope, so the guard's `Drop` is a no-op (it checks the flag).
//!
//! ```text
//!   open frame received
//!     → ensure_active
//!     → registry.get (→ Conflict if no sandbox)
//!     → acquire_shell (warn-and-continue on failure)
//!     → proxy_shell (→ Unavailable on failure, release + close)
//!     → pump loop (b2t || t2b, select!)
//!     → explicit release_shell
//!   [drop of ShellLeaseGuard is a no-op because released=true]
//! ```

use std::sync::Arc;

use bytes::Bytes;
use engram_core::types::shell::{ShellClose, ShellFrame, ShellTunnel};
use engram_protocol::app;
use futures::StreamExt as _;
use tonic::{Request, Response, Status};

use super::{auth, into_status, parse_session_id, BoxStream};
use crate::state::SharedState;

pub struct AppShellRelayService {
    pub state: SharedState,
    pub auth: Arc<auth::BearerAuth>,
}

/// RAII guard that ensures `release_shell` is always called even when
/// tonic cancels the future mid-bridge.  `Drop` spawns a fire-and-forget
/// task rather than blocking (async `Drop` is not supported in Rust).
struct ShellLeaseGuard {
    host: Arc<dyn engram_core::traits::HostClient>,
    sandbox_id: engram_core::SandboxId,
    released: bool,
}

impl ShellLeaseGuard {
    fn new(
        host: Arc<dyn engram_core::traits::HostClient>,
        sandbox_id: engram_core::SandboxId,
    ) -> Self {
        Self {
            host,
            sandbox_id,
            released: false,
        }
    }

    /// Explicit release on the normal exit path.  Marks the guard as
    /// released and spawns the async `release_shell` call (so this method
    /// can be called from non-async contexts and from `Drop`-alike paths).
    /// `Drop` is then a no-op because `released == true`.
    fn release(mut self) {
        self.released = true;
        let host = self.host.clone();
        let sandbox_id = self.sandbox_id;
        tokio::spawn(async move {
            if let Err(e) = host.release_shell(sandbox_id).await {
                tracing::warn!(
                    %sandbox_id,
                    error = %e,
                    "ShellRelay: release_shell failed on explicit release",
                );
            }
        });
    }
}

impl Drop for ShellLeaseGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        // Tonic cancelled mid-bridge — spawn a detached task so the
        // async release can still run (Drop cannot .await).
        let host = self.host.clone();
        let sandbox_id = self.sandbox_id;
        tokio::spawn(async move {
            if let Err(e) = host.release_shell(sandbox_id).await {
                tracing::warn!(
                    %sandbox_id,
                    error = %e,
                    "ShellRelay: release_shell failed in Drop guard (post-cancel cleanup)",
                );
            }
        });
    }
}

// EVERY RPC body starts with self.auth.check(&req)? — see auth.rs and the convention test.
#[tonic::async_trait]
impl app::shell_relay_service_server::ShellRelayService for AppShellRelayService {
    type RelayStream = BoxStream<app::RelayShellResponse>;

    async fn relay(
        &self,
        req: Request<tonic::Streaming<app::RelayShellRequest>>,
    ) -> Result<Response<Self::RelayStream>, Status> {
        self.auth.check(&req)?;

        let mut inbound = req.into_inner();

        // ---- 1. Expect an `open` frame as the first message ----------
        let first = inbound
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("relay stream ended before open frame"))?
            .map_err(|e| Status::internal(format!("relay recv open: {e}")))?;

        let session_id_str = match first.frame {
            Some(app::relay_shell_request::Frame::Open(o)) => o.session_id,
            other => {
                return Err(Status::invalid_argument(format!(
                    "first relay frame must be open, got {:?}",
                    other.map(|f| relay_frame_name(&f))
                )));
            }
        };

        let session_id = parse_session_id(&session_id_str)?;

        // ---- 2. Session lifecycle: ensure active (auto-resume Idle) --
        crate::api::snapshot::ensure_active(&self.state, session_id)
            .await
            .map_err(into_status)?;

        let sandbox_id = self.state.registry.get(session_id).ok_or_else(|| {
            Status::failed_precondition(
                "session has no live sandbox after auto-resume; \
                 try again or `engram session resume <id>` and retry",
            )
        })?;

        // ---- 3. acquire_shell (warn-and-continue, non-fatal) ---------
        let host = self.state.services.host.clone();
        if let Err(e) = host.acquire_shell(sandbox_id).await {
            tracing::warn!(
                %session_id,
                %sandbox_id,
                error = %e,
                "ShellRelay: acquire_shell failed; relay still opens but idle eviction may race",
            );
        }

        // ---- 4. proxy_shell → open host tunnel -----------------------
        let tunnel = match host.proxy_shell(sandbox_id).await {
            Ok(t) => t,
            Err(e) => {
                // Release the pin we just acquired before returning.
                let guard = ShellLeaseGuard::new(host.clone(), sandbox_id);
                guard.release();
                return Err(Status::unavailable(format!(
                    "proxy_shell tunnel open failed: {e}"
                )));
            }
        };

        // ---- 5. Bridge: inbound gRPC ↔ ShellTunnel ------------------
        // The lease guard ensures release_shell runs even if tonic cancels.
        let lease = ShellLeaseGuard::new(host.clone(), sandbox_id);

        let (outbound_stream, inbound_stream) =
            build_relay_stream(session_id, sandbox_id, inbound, tunnel, lease);

        // `outbound_stream` and `inbound_stream` are conceptually one
        // merged stream for our response.  We build the final stream in
        // build_relay_stream above.
        Ok(Response::new(Box::pin(
            outbound_stream.chain(inbound_stream),
        )))
    }
}

/// Human-readable variant name for error messages.
fn relay_frame_name(f: &app::relay_shell_request::Frame) -> &'static str {
    match f {
        app::relay_shell_request::Frame::Open(_) => "open",
        app::relay_shell_request::Frame::Text(_) => "text",
        app::relay_shell_request::Frame::Binary(_) => "binary",
        app::relay_shell_request::Frame::Ping(_) => "ping",
        app::relay_shell_request::Frame::Pong(_) => "pong",
        app::relay_shell_request::Frame::Close(_) => "close",
    }
}

/// Map a proto `RelayShellRequest` frame to a `ShellFrame`.
fn proto_to_shell_frame(frame: app::relay_shell_request::Frame) -> ShellFrame {
    match frame {
        app::relay_shell_request::Frame::Open(_) => {
            // open is consumed before we enter the pump; should not appear here.
            ShellFrame::Close(None)
        }
        app::relay_shell_request::Frame::Text(t) => ShellFrame::Text(t),
        app::relay_shell_request::Frame::Binary(b) => ShellFrame::Binary(Bytes::from(b)),
        app::relay_shell_request::Frame::Ping(b) => ShellFrame::Ping(Bytes::from(b)),
        app::relay_shell_request::Frame::Pong(b) => ShellFrame::Pong(Bytes::from(b)),
        app::relay_shell_request::Frame::Close(c) => ShellFrame::Close(Some(ShellClose {
            code: c.code as u16,
            reason: c.reason,
        })),
    }
}

/// Map a `ShellFrame` to a proto `RelayShellResponse`.
fn shell_frame_to_proto(frame: ShellFrame) -> app::RelayShellResponse {
    use app::relay_shell_response::Frame;
    let f = match frame {
        ShellFrame::Text(t) => Frame::Text(t),
        ShellFrame::Binary(b) => Frame::Binary(b.into()),
        ShellFrame::Ping(b) => Frame::Ping(b.into()),
        ShellFrame::Pong(b) => Frame::Pong(b.into()),
        ShellFrame::Close(Some(c)) => Frame::Close(app::ShellClose {
            code: c.code as u32,
            reason: c.reason,
        }),
        ShellFrame::Close(None) => Frame::Close(app::ShellClose {
            code: 1000,
            reason: String::new(),
        }),
    };
    app::RelayShellResponse { frame: Some(f) }
}

/// Build the two halves of the bidi bridge as a single chained stream.
///
/// Returns `(ttyd_to_client_stream, empty_end_stream)`.  The actual
/// pump runs inside an async task; the returned stream drains a
/// `tokio::sync::mpsc` that the pump fills.  When the pump finishes
/// (either side closes), the channel closes and the stream ends.
fn build_relay_stream(
    session_id: engram_core::SessionId,
    sandbox_id: engram_core::SandboxId,
    mut grpc_inbound: tonic::Streaming<app::RelayShellRequest>,
    tunnel: ShellTunnel,
    lease: ShellLeaseGuard,
) -> (
    impl futures::stream::Stream<Item = Result<app::RelayShellResponse, Status>> + Send,
    futures::stream::Empty<Result<app::RelayShellResponse, Status>>,
) {
    // Channel capacity: 64 frames, matching ShellTunnel's own capacity.
    let (resp_tx, resp_rx) =
        tokio::sync::mpsc::channel::<Result<app::RelayShellResponse, Status>>(64);

    let ShellTunnel {
        outbound: tunnel_out_tx,
        inbound: mut tunnel_in_rx,
    } = tunnel;

    tokio::spawn(async move {
        // Move the lease into the task — it will be released when the task ends.
        let lease = lease;

        // gRPC client → ttyd: pump inbound frames to the tunnel outbound channel.
        let g2t_tx = tunnel_out_tx.clone();
        let g2t_resp_tx = resp_tx.clone();
        let g2t = async move {
            while let Some(msg) = grpc_inbound.next().await {
                match msg {
                    Err(e) => {
                        // gRPC transport error — close the bridge.
                        tracing::debug!(
                            %session_id,
                            error = %e,
                            "ShellRelay: gRPC inbound error; closing bridge",
                        );
                        break;
                    }
                    Ok(req) => {
                        let Some(frame) = req.frame else { continue };
                        let shell_frame = proto_to_shell_frame(frame);
                        if g2t_tx.send(shell_frame).await.is_err() {
                            // Tunnel outbound closed (host side tore down).
                            break;
                        }
                    }
                }
            }
        };

        // ttyd → gRPC client: drain the tunnel inbound channel.
        let t2g_resp_tx = resp_tx;
        let t2g = async move {
            while let Some(frame) = tunnel_in_rx.recv().await {
                let proto = shell_frame_to_proto(frame);
                if t2g_resp_tx.send(Ok(proto)).await.is_err() {
                    // Response channel closed (client disconnected).
                    break;
                }
            }
        };

        // Run both halves concurrently; whichever finishes first tears down.
        tokio::select! {
            _ = g2t => {}
            _ = t2g => {}
        }

        // Normal exit — explicit release (spawns async release_shell internally).
        lease.release();

        tracing::debug!(
            %session_id,
            %sandbox_id,
            "ShellRelay: bridge ended, lease released",
        );
        // g2t_resp_tx clone was dropped; resp_rx will drain and end.
        let _ = g2t_resp_tx;
    });

    let out_stream = tokio_stream::wrappers::ReceiverStream::new(resp_rx);
    (out_stream, futures::stream::empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proto_to_shell_frame_text_roundtrip() {
        let frame = app::relay_shell_request::Frame::Text("hello".to_string());
        let shell = proto_to_shell_frame(frame);
        assert!(matches!(shell, ShellFrame::Text(t) if t == "hello"));
    }

    #[test]
    fn proto_to_shell_frame_close_with_fields() {
        let frame = app::relay_shell_request::Frame::Close(app::ShellClose {
            code: 1001,
            reason: "going away".into(),
        });
        let shell = proto_to_shell_frame(frame);
        match shell {
            ShellFrame::Close(Some(c)) => {
                assert_eq!(c.code, 1001);
                assert_eq!(c.reason, "going away");
            }
            other => panic!("expected ShellFrame::Close(Some(_)), got {other:?}"),
        }
    }

    #[test]
    fn shell_frame_to_proto_text_roundtrip() {
        let frame = ShellFrame::Text("world".to_string());
        let proto = shell_frame_to_proto(frame);
        assert_eq!(
            proto.frame,
            Some(app::relay_shell_response::Frame::Text("world".to_string()))
        );
    }

    #[test]
    fn shell_frame_to_proto_close_none_defaults_to_1000() {
        let proto = shell_frame_to_proto(ShellFrame::Close(None));
        match proto.frame {
            Some(app::relay_shell_response::Frame::Close(c)) => {
                assert_eq!(c.code, 1000);
                assert_eq!(c.reason, "");
            }
            other => panic!("expected Close frame, got {other:?}"),
        }
    }
}
