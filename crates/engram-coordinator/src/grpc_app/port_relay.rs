//! `PortRelayService` over gRPC (ADR 0064).
//!
//! The orchestrator uses this bidi-streaming RPC to bridge a vanity-
//! subdomain preview connection through the coordinator into an
//! arbitrary guest TCP port. It is the raw-byte sibling of
//! [`super::shell_relay`]: the first inbound frame MUST be
//! `open { session_id, port }`; subsequent frames are opaque `data`
//! byte chunks (or `close`).
//!
//! The session-lifecycle contract is identical to the shell relay:
//! `ensure_active` auto-resumes Idle/Evacuating sessions; the
//! interactive-attachment pin (`acquire_shell`/`release_shell`, shared
//! with the shell — see ADR 0064) holds the sandbox against idle
//! eviction while a preview connection is live; `proxy_port` opens the
//! host-agent tunnel; the pin is released on every exit path, including
//! a tonic mid-bridge cancel (via the RAII guard, exactly as
//! `shell_relay` does it).

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use engram_core::traits::HostClient;
use engram_core::types::port::PortTunnel;
use engram_protocol::app;
use futures::StreamExt as _;
use tonic::{Request, Response, Status};

use super::{auth, into_status, parse_session_id, BoxStream};
use crate::state::SharedState;

/// Renew the interactive-attachment pin this often while a preview
/// connection is live, so a long-lived but low-traffic preview isn't
/// reaped by the host's stale-pin sweep (host-agent
/// `harness::SHELL_PIN_STALE_AGE` = 300s) and idle-evicted out from
/// under the viewer. Matches the shell pin's intended renew cadence
/// (`harness::SHELL_PIN_RENEW_INTERVAL` = 60s); kept well below the
/// stale age so a few dropped renewals don't trip the sweep. (Not
/// importing the host-agent const to avoid a coordinator→host-agent
/// build coupling for one number.)
const PIN_RENEW_INTERVAL: Duration = Duration::from_secs(60);

pub struct AppPortRelayService {
    pub state: SharedState,
    pub auth: Arc<auth::BearerAuth>,
}

/// RAII guard mirroring `shell_relay::ShellLeaseGuard`: releases the
/// interactive-attachment pin even when tonic cancels the future
/// mid-bridge (async `Drop` isn't supported, so `Drop` spawns a
/// fire-and-forget release).
struct PortLeaseGuard {
    host: Arc<dyn engram_core::traits::HostClient>,
    sandbox_id: engram_core::SandboxId,
    released: bool,
    /// ADR 0066: the per-session preview slot, released when this guard drops
    /// (i.e. on every connection-exit path, exactly with the pin).
    _preview: Option<crate::state::PreviewPermit>,
}

impl PortLeaseGuard {
    fn new(
        host: Arc<dyn engram_core::traits::HostClient>,
        sandbox_id: engram_core::SandboxId,
        preview: Option<crate::state::PreviewPermit>,
    ) -> Self {
        Self {
            host,
            sandbox_id,
            released: false,
            _preview: preview,
        }
    }

    /// Explicit release on the normal exit path; `Drop` then no-ops.
    fn release(mut self) {
        self.released = true;
        let host = self.host.clone();
        let sandbox_id = self.sandbox_id;
        tokio::spawn(async move {
            if let Err(e) = host.release_shell(sandbox_id).await {
                tracing::warn!(
                    %sandbox_id,
                    error = %e,
                    "PortRelay: release pin failed on explicit release",
                );
            }
        });
    }
}

impl Drop for PortLeaseGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let host = self.host.clone();
        let sandbox_id = self.sandbox_id;
        tokio::spawn(async move {
            if let Err(e) = host.release_shell(sandbox_id).await {
                tracing::warn!(
                    %sandbox_id,
                    error = %e,
                    "PortRelay: release pin failed in Drop guard (post-cancel cleanup)",
                );
            }
        });
    }
}

// EVERY RPC body starts with self.auth.check(&req)? — see auth.rs and the convention test.
#[tonic::async_trait]
impl app::port_relay_service_server::PortRelayService for AppPortRelayService {
    type RelayStream = BoxStream<app::RelayPortResponse>;

    async fn relay(
        &self,
        req: Request<tonic::Streaming<app::RelayPortRequest>>,
    ) -> Result<Response<Self::RelayStream>, Status> {
        self.auth.check(&req)?;

        let mut inbound = req.into_inner();

        // ---- 1. Expect an `open` frame as the first message ----------
        let first = inbound
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("relay stream ended before open frame"))?
            .map_err(|e| Status::internal(format!("relay recv open: {e}")))?;

        let (session_id_str, port_u32) = match first.frame {
            Some(app::relay_port_request::Frame::Open(o)) => (o.session_id, o.port),
            other => {
                return Err(Status::invalid_argument(format!(
                    "first relay frame must be open, got {:?}",
                    other.map(|f| relay_frame_name(&f))
                )));
            }
        };

        let session_id = parse_session_id(&session_id_str)?;
        let port = u16::try_from(port_u32)
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| {
                Status::invalid_argument(format!(
                    "relay open: port {port_u32} out of range (1..=65535)"
                ))
            })?;

        // ADR 0066: fail fast if this session is already at its concurrent-
        // preview-connection cap — before auto-resume, sandbox resolution, or
        // touching the host/guest. The permit is held for the connection's
        // lifetime by the bridge's lease guard (released on every exit path);
        // on any early return here it drops and releases immediately.
        let preview_permit = self
            .state
            .preview_conns
            .try_acquire(session_id)
            .ok_or_else(|| {
                Status::resource_exhausted(format!(
                    "session {session_id} is at its concurrent preview-connection cap"
                ))
            })?;

        // ---- 2. Session lifecycle: ensure active (auto-resume Idle) --
        crate::api::snapshot::ensure_active(&self.state, session_id)
            .await
            .map_err(into_status)?;

        // ADR 0047: PG is the routing authority — resolve the bound
        // sandbox from the session row (works on any replica).
        let sandbox_id = self
            .state
            .resolve_sandbox(session_id)
            .await
            .ok_or_else(|| {
                into_status(crate::error::ApiError::Conflict(
                    "session has no live sandbox after auto-resume; try again".into(),
                ))
            })?;

        // ---- 3. Pin against idle eviction (warn-and-continue) --------
        let host = self.state.services.host.clone();
        if let Err(e) = host.acquire_shell(sandbox_id).await {
            tracing::warn!(
                %session_id,
                %sandbox_id,
                error = %e,
                "PortRelay: acquire pin failed; relay still opens but idle eviction may race",
            );
        }

        // ---- 4. proxy_port → open host tunnel -----------------------
        let tunnel = match host.proxy_port(sandbox_id, port).await {
            Ok(t) => t,
            Err(e) => {
                // Release the pin we just acquired before returning. The preview
                // permit drops with `preview_permit` at the return below.
                let guard = PortLeaseGuard::new(host.clone(), sandbox_id, None);
                guard.release();
                return Err(Status::unavailable(format!(
                    "proxy_port tunnel open failed: {e}"
                )));
            }
        };

        // ---- 5. Bridge: inbound gRPC ↔ PortTunnel -------------------
        // The lease guard now also owns the preview slot for the connection's
        // lifetime (released on every exit path, exactly with the pin).
        let lease = PortLeaseGuard::new(host.clone(), sandbox_id, Some(preview_permit));
        let relay_stream = build_relay_stream(session_id, sandbox_id, host, inbound, tunnel, lease);

        Ok(Response::new(Box::pin(relay_stream)))
    }
}

/// Human-readable variant name for error messages.
fn relay_frame_name(f: &app::relay_port_request::Frame) -> &'static str {
    match f {
        app::relay_port_request::Frame::Open(_) => "open",
        app::relay_port_request::Frame::Data(_) => "data",
        app::relay_port_request::Frame::Close(_) => "close",
    }
}

/// Build the bidi bridge as a single stream. The pump runs in a task and
/// fills an mpsc that the returned stream drains; when either side
/// closes, the channel closes and the stream ends (and the lease guard
/// releases the pin). Mirrors `shell_relay::build_relay_stream`.
fn build_relay_stream(
    session_id: engram_core::SessionId,
    sandbox_id: engram_core::SandboxId,
    host: Arc<dyn HostClient>,
    mut grpc_inbound: tonic::Streaming<app::RelayPortRequest>,
    tunnel: PortTunnel,
    lease: PortLeaseGuard,
) -> impl futures::stream::Stream<Item = Result<app::RelayPortResponse, Status>> + Send {
    let (resp_tx, resp_rx) =
        tokio::sync::mpsc::channel::<Result<app::RelayPortResponse, Status>>(64);

    let PortTunnel {
        outbound: tunnel_out_tx,
        inbound: mut tunnel_in_rx,
    } = tunnel;

    tokio::spawn(async move {
        let lease = lease;

        // gRPC client → guest: pump inbound Data frames into the tunnel.
        let g2t_tx = tunnel_out_tx.clone();
        let g2t_resp_tx = resp_tx.clone();
        let g2t = async move {
            while let Some(msg) = grpc_inbound.next().await {
                match msg {
                    Err(e) => {
                        tracing::debug!(
                            %session_id,
                            error = %e,
                            "PortRelay: gRPC inbound error; closing bridge",
                        );
                        break;
                    }
                    Ok(req) => match req.frame {
                        Some(app::relay_port_request::Frame::Data(d)) => {
                            if g2t_tx.send(Bytes::from(d)).await.is_err() {
                                break; // tunnel outbound closed (host tore down)
                            }
                        }
                        Some(app::relay_port_request::Frame::Close(_)) | None => break,
                        Some(app::relay_port_request::Frame::Open(_)) => {
                            // `open` is only valid as the first frame.
                            let _ = g2t_resp_tx
                                .send(Err(Status::invalid_argument(
                                    "open frame only valid as the first frame",
                                )))
                                .await;
                            break;
                        }
                    },
                }
            }
        };

        // guest → gRPC client: drain tunnel inbound into Data responses.
        let t2g_resp_tx = resp_tx;
        let t2g = async move {
            while let Some(chunk) = tunnel_in_rx.recv().await {
                let resp = app::RelayPortResponse {
                    frame: Some(app::relay_port_response::Frame::Data(chunk.to_vec())),
                };
                if t2g_resp_tx.send(Ok(resp)).await.is_err() {
                    break; // client disconnected
                }
            }
        };

        // Keep the interactive-attachment pin fresh for the life of the
        // connection so the host's stale-pin sweep doesn't reap it (and
        // make the sandbox idle-evictable) under a long-lived preview.
        // Never completes on its own; cancelled when g2t/t2g finishes.
        let renew = async move {
            let mut tick = tokio::time::interval(PIN_RENEW_INTERVAL);
            tick.tick().await; // consume the immediate first tick
            loop {
                tick.tick().await;
                if let Err(e) = host.renew_shell(sandbox_id).await {
                    tracing::debug!(
                        %sandbox_id,
                        error = %e,
                        "PortRelay: renew pin failed (will retry next tick)",
                    );
                }
            }
        };

        tokio::select! {
            _ = g2t => {}
            _ = t2g => {}
            _ = renew => {}
        }

        // Normal exit — explicit release (spawns async release internally).
        lease.release();

        tracing::debug!(
            %session_id,
            %sandbox_id,
            "PortRelay: bridge ended, pin released",
        );
    });

    tokio_stream::wrappers::ReceiverStream::new(resp_rx)
}
