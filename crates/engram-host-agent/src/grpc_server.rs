//! Host-agent's gRPC `HostService` server (ADR 0013).
//!
//! Implements the proto-generated `HostService` trait by delegating
//! to the local `LocalHostClient`. One in-proc backend, dispatched
//! into by coord pods over HTTP/2.
//!
//! The server is bound by `boot(...)` to a TCP listener (typically
//! `0.0.0.0:9101`) and runs until shutdown. Errors mid-RPC become
//! `tonic::Status` codes mirroring the WS path's `RemoteError`
//! mapping.
//!
//! `result_large_err` is allowed module-wide because `tonic::Status`
//! is the only error shape for trait methods and helpers — boxing
//! the Status doesn't compose with tonic's signatures.

#![allow(clippy::result_large_err)]

use std::sync::Arc;

use engram_core::traits::HostClient;
use engram_core::SandboxError;
use engram_protocol::admin::HostAdminHandler;
use engram_protocol::grpc::host_service_server::{HostService, HostServiceServer};
use engram_protocol::grpc::{
    lease_warm_response::Outcome as PbLeaseOutcome, ApplyEgressPolicyRequest,
    BindHarnessSessionRequest, CreateSandboxRequest, CreateSandboxResponse, Empty, ExecExit,
    ExecFrame, ExecStartRequest, GuestIpResponse, LaunchWarmRequest, LeaseWarmRequest,
    LeaseWarmResponse, ListSandboxesResponse, ProxyShellFrame, ProxyShellKind,
    ReapMaterializeDirRequest, ReapMaterializeDirResponse, RestoreRequest, SandboxIdMessage,
    SendHarnessPromptRequest, SnapshotResponse, StaleTemplate as PbStaleTemplate,
    StartAgentRequest, UnbindHarnessSessionRequest, WarmSlotCount as PbWarmSlotCount,
    WarmSlotsResponse,
};
use engram_protocol::wire::{WireExecRequest, WireReapStats};
use futures::Stream;
use std::pin::Pin;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

/// Concrete `HostService` impl. Holds the in-proc
/// `Arc<dyn HostClient>` (typically a `LocalHostClient` wrapping
/// the host's SandboxBackend + HarnessHub). Optional
/// `HostAdminHandler` services ReapMaterializeDir.
pub struct HostServiceImpl {
    inner: Arc<dyn HostClient>,
    admin: Option<Arc<dyn HostAdminHandler>>,
}

impl HostServiceImpl {
    pub fn new(inner: Arc<dyn HostClient>) -> Self {
        Self { inner, admin: None }
    }

    pub fn with_admin_handler(mut self, admin: Arc<dyn HostAdminHandler>) -> Self {
        self.admin = Some(admin);
        self
    }
}

/// Boot a tonic `Server` bound to `listen_addr` and serve the
/// `HostService` until the caller drops the returned future. The
/// JoinHandle terminates with `Err` on bind failure or panic; a
/// clean Ctrl-C just drops the future, which the runtime collects.
pub async fn boot(
    listen_addr: std::net::SocketAddr,
    inner: Arc<dyn HostClient>,
    admin: Option<Arc<dyn HostAdminHandler>>,
) -> Result<(), tonic::transport::Error> {
    let mut svc = HostServiceImpl::new(inner);
    if let Some(a) = admin {
        svc = svc.with_admin_handler(a);
    }
    tracing::info!(addr = %listen_addr, "gRPC HostService listening");
    tonic::transport::Server::builder()
        // ADR 0013: HTTP/2 multiplexes many concurrent streams over
        // one TCP connection per coord pod. 256 is well above the
        // running_sandboxes ceiling — we don't expect to hit it.
        .concurrency_limit_per_connection(256)
        .add_service(HostServiceServer::new(svc))
        .serve(listen_addr)
        .await
}

#[tonic::async_trait]
impl HostService for HostServiceImpl {
    type ExecStartStream = Pin<Box<dyn Stream<Item = Result<ExecFrame, Status>> + Send + 'static>>;
    type ProxyShellStream =
        Pin<Box<dyn Stream<Item = Result<ProxyShellFrame, Status>> + Send + 'static>>;

    async fn ping(&self, _req: Request<Empty>) -> Result<Response<Empty>, Status> {
        Ok(Response::new(Empty {}))
    }

    async fn create_sandbox(
        &self,
        req: Request<CreateSandboxRequest>,
    ) -> Result<Response<CreateSandboxResponse>, Status> {
        let spec = decode_bincode(&req.into_inner().spec_bincode, "SandboxSpec")?;
        let sandbox_id = self.inner.create(spec).await.map_err(sandbox_to_status)?;
        Ok(Response::new(CreateSandboxResponse {
            sandbox_id: sandbox_id.as_uuid().as_bytes().to_vec(),
        }))
    }

    async fn destroy_sandbox(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<Empty>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        self.inner.destroy(id).await.map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn list_sandboxes(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<ListSandboxesResponse>, Status> {
        let ids = self.inner.list().await.map_err(sandbox_to_status)?;
        Ok(Response::new(ListSandboxesResponse {
            sandbox_ids: ids
                .into_iter()
                .map(|id| id.as_uuid().as_bytes().to_vec())
                .collect(),
        }))
    }

    async fn snapshot(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<SnapshotResponse>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        let metadata = self.inner.snapshot(id).await.map_err(sandbox_to_status)?;
        Ok(Response::new(SnapshotResponse {
            metadata_bincode: encode_bincode(&metadata, "SnapshotMetadata")?,
        }))
    }

    async fn commit_snapshot(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<Empty>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        self.inner
            .commit_snapshot(id)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn abort_snapshot(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<Empty>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        self.inner
            .abort_snapshot(id)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn restore(
        &self,
        req: Request<RestoreRequest>,
    ) -> Result<Response<SandboxIdMessage>, Status> {
        let metadata = decode_bincode(&req.into_inner().metadata_bincode, "SnapshotMetadata")?;
        let id = self
            .inner
            .restore(metadata)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(SandboxIdMessage {
            uuid: id.as_uuid().as_bytes().to_vec(),
        }))
    }

    async fn guest_ip(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<GuestIpResponse>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        let ip = self.inner.guest_ip(id).await;
        Ok(Response::new(GuestIpResponse { ip }))
    }

    async fn bind_harness_session(
        &self,
        req: Request<BindHarnessSessionRequest>,
    ) -> Result<Response<Empty>, Status> {
        let r = req.into_inner();
        let session_id = decode_session_id(&r.session_id)?;
        let sandbox_id = decode_sandbox_id(&r.sandbox_id)?;
        self.inner.bind_session(session_id, sandbox_id).await;
        Ok(Response::new(Empty {}))
    }

    async fn unbind_harness_session(
        &self,
        req: Request<UnbindHarnessSessionRequest>,
    ) -> Result<Response<Empty>, Status> {
        let session_id = decode_session_id(&req.into_inner().session_id)?;
        self.inner.unbind_session(session_id).await;
        Ok(Response::new(Empty {}))
    }

    async fn send_harness_prompt(
        &self,
        req: Request<SendHarnessPromptRequest>,
    ) -> Result<Response<Empty>, Status> {
        let r = req.into_inner();
        let sandbox_id = decode_sandbox_id(&r.sandbox_id)?;
        self.inner
            .send_prompt(sandbox_id, r.text)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    /// ADR 0013: bundled policy + start. One trait call applies the
    /// `SessionEgressPolicy` to the host's egress proxy registry
    /// BEFORE spawning the agent process. Atomic by construction.
    ///
    /// This entry-point serves the **cold-create** path: coord
    /// calls `StartAgent` after `Create` returns, the in-guest
    /// bootstrap may not be accept()'ing yet, and the dial blocks
    /// until kernel + engram-init + ext4 mount complete. The
    /// warm-lease path doesn't reach here — it goes through
    /// `WarmPool::launch` (which has its own histogram emission
    /// labelled `kind="warm"`).
    async fn start_agent(
        &self,
        req: Request<StartAgentRequest>,
    ) -> Result<Response<Empty>, Status> {
        let r = req.into_inner();
        let sandbox_id = decode_sandbox_id(&r.sandbox_id)?;
        let agent = decode_bincode(&r.agent_bincode, "AgentSpec")?;
        let policy = decode_bincode(&r.policy_bincode, "SessionEgressPolicy")?;
        let phase_start = std::time::Instant::now();
        let result = self
            .inner
            .start_agent(sandbox_id, agent, policy)
            .await
            .map_err(sandbox_to_status);
        let outcome = if result.is_ok() {
            "success"
        } else {
            "fc_error"
        };
        metrics::histogram!(
            crate::metrics::SANDBOX_BOOT_SECONDS,
            "phase" => "agent_handshake",
            "outcome" => outcome,
            "kind" => "cold",
        )
        .record(phase_start.elapsed().as_secs_f64());
        result?;
        Ok(Response::new(Empty {}))
    }

    async fn apply_egress_policy(
        &self,
        req: Request<ApplyEgressPolicyRequest>,
    ) -> Result<Response<Empty>, Status> {
        let policy = decode_bincode(&req.into_inner().policy_bincode, "SessionEgressPolicy")?;
        self.inner
            .apply_egress_policy(policy)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn acquire_shell(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<Empty>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        self.inner
            .acquire_shell(id)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn release_shell(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<Empty>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        self.inner
            .release_shell(id)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    /// ADR 0014: atomic take from the warm pool. The three response
    /// outcomes (granted / stale / no_capacity) come from the
    /// HostClient trait and map 1:1 onto the proto oneof.
    async fn lease_warm_sandbox(
        &self,
        req: Request<LeaseWarmRequest>,
    ) -> Result<Response<LeaseWarmResponse>, Status> {
        use engram_core::traits::host_client::WarmLeaseOutcome;
        let r = req.into_inner();
        let template_ref = decode_template_ref(&r.template_ref)?;
        let outcome = self
            .inner
            .lease_warm_sandbox(template_ref)
            .await
            .map_err(sandbox_to_status)?;
        let pb_outcome = match outcome {
            WarmLeaseOutcome::Granted(id) => {
                PbLeaseOutcome::SandboxId(id.as_uuid().as_bytes().to_vec())
            }
            WarmLeaseOutcome::Stale { current_ref } => PbLeaseOutcome::Stale(PbStaleTemplate {
                current_ref: current_ref.as_uuid().as_bytes().to_vec(),
            }),
            WarmLeaseOutcome::NoCapacity => PbLeaseOutcome::NoCapacity(Empty {}),
        };
        Ok(Response::new(LeaseWarmResponse {
            outcome: Some(pb_outcome),
        }))
    }

    /// ADR 0014: activate a leased warm sandbox by pushing a fresh
    /// BootstrapLaunch (carrying the per-session agent + env) and
    /// applying the egress policy to the host's proxy registry.
    async fn launch_warm_sandbox(
        &self,
        req: Request<LaunchWarmRequest>,
    ) -> Result<Response<Empty>, Status> {
        let r = req.into_inner();
        let sandbox_id = decode_sandbox_id(&r.sandbox_id)?;
        let agent = decode_bincode(&r.agent_bincode, "AgentSpec")?;
        let policy = decode_bincode(&r.policy_bincode, "SessionEgressPolicy")?;
        // ADR 0014 M1.12: proto3 string can't be `Option`; empty
        // string on the wire means "no harness swap" (e.g.,
        // `kind = none` sessions).
        let harness_pack_uri = if r.harness_pack_uri.is_empty() {
            None
        } else {
            Some(r.harness_pack_uri)
        };
        let harness_name = if r.harness_name.is_empty() {
            None
        } else {
            Some(r.harness_name)
        };
        self.inner
            .launch_warm_sandbox(sandbox_id, agent, policy, harness_pack_uri, harness_name)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    /// ADR 0014: inventory query for ops + scheduler shortcuts.
    async fn list_warm_slots(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<WarmSlotsResponse>, Status> {
        let slots = self
            .inner
            .list_warm_slots()
            .await
            .map_err(sandbox_to_status)?;
        let pb_slots = slots
            .into_iter()
            .map(|s| PbWarmSlotCount {
                template_ref: s.template_ref.as_uuid().as_bytes().to_vec(),
                available: s.available,
                target: s.target,
                refill_failures_since_last: s.refill_failures_since_last,
                last_error_class: s.last_error_class,
            })
            .collect();
        Ok(Response::new(WarmSlotsResponse { slots: pb_slots }))
    }

    async fn reap_materialize_dir(
        &self,
        req: Request<ReapMaterializeDirRequest>,
    ) -> Result<Response<ReapMaterializeDirResponse>, Status> {
        let Some(admin) = self.admin.as_ref() else {
            return Err(Status::unimplemented(
                "host did not register a HostAdminHandler; ReapMaterializeDir unsupported",
            ));
        };
        let r = req.into_inner();
        let live: Vec<uuid::Uuid> = r
            .live_disk_manifest_ids
            .iter()
            .map(|b| {
                if b.len() != 16 {
                    Err(Status::invalid_argument(format!(
                        "live_disk_manifest_id must be 16 bytes, got {}",
                        b.len()
                    )))
                } else {
                    let mut buf = [0u8; 16];
                    buf.copy_from_slice(b);
                    Ok(uuid::Uuid::from_bytes(buf))
                }
            })
            .collect::<Result<_, _>>()?;
        let stats: WireReapStats = admin
            .reap_materialize_dir(r.min_age_secs, live)
            .await
            .map_err(|e| Status::internal(format!("reap_materialize_dir: {e}")))?;
        Ok(Response::new(ReapMaterializeDirResponse {
            stats_bincode: encode_bincode(&stats, "WireReapStats")?,
        }))
    }

    /// Server-streaming exec. First frame is `started` (carries the
    /// host-assigned `exec_id`); subsequent frames carry stdout /
    /// stderr bytes; terminal frame is `exit` (always exactly one).
    async fn exec_start(
        &self,
        req: Request<ExecStartRequest>,
    ) -> Result<Response<Self::ExecStartStream>, Status> {
        let r = req.into_inner();
        let sandbox_id = decode_sandbox_id(&r.sandbox_id)?;
        let wire: WireExecRequest = decode_bincode(&r.request_bincode, "WireExecRequest")?;
        let request = wire.into_engine();

        let mut stream = self
            .inner
            .exec_stream(sandbox_id, request)
            .await
            .map_err(sandbox_to_status)?;

        // mpsc channel feeds the gRPC stream out. We pump the
        // backend's `ExecStream` events into it; the stream ends
        // when the backend sends `Exit` or its events channel
        // closes (treat-as-Exit-None per the WS path's drain
        // logic).
        let (tx, rx) = mpsc::channel::<Result<ExecFrame, Status>>(64);

        // First frame: `started` with the assigned exec_id.
        let started = ExecFrame {
            frame: Some(engram_protocol::grpc::exec_frame::Frame::Started(
                stream.exec_id.clone(),
            )),
        };
        if tx.send(Ok(started)).await.is_err() {
            // Client already gave up; nothing to do.
            return Err(Status::cancelled("client closed stream before Started"));
        }

        tokio::spawn(async move {
            use engram_core::types::sandbox::ExecEvent;
            use futures::StreamExt;
            while let Some(ev) = stream.events.next().await {
                let frame = match ev {
                    ExecEvent::Stdout(b) => ExecFrame {
                        frame: Some(engram_protocol::grpc::exec_frame::Frame::Stdout(b.to_vec())),
                    },
                    ExecEvent::Stderr(b) => ExecFrame {
                        frame: Some(engram_protocol::grpc::exec_frame::Frame::Stderr(b.to_vec())),
                    },
                    ExecEvent::Exit(status) => {
                        let frame = ExecFrame {
                            frame: Some(engram_protocol::grpc::exec_frame::Frame::Exit(ExecExit {
                                status,
                            })),
                        };
                        let _ = tx.send(Ok(frame)).await;
                        return;
                    }
                };
                if tx.send(Ok(frame)).await.is_err() {
                    // Client dropped the stream — stop pumping.
                    return;
                }
            }
            // Backend events channel ended without an explicit Exit
            // frame. Synthesize Exit(None) so the demuxer downstream
            // terminates cleanly — matches the WS path's
            // `drain_exec_stream` behavior.
            let _ = tx
                .send(Ok(ExecFrame {
                    frame: Some(engram_protocol::grpc::exec_frame::Frame::Exit(ExecExit {
                        status: None,
                    })),
                }))
                .await;
        });

        let out_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(out_stream) as Self::ExecStartStream))
    }

    /// ADR 0014 issue #6: bidi WS-frame tunnel. First inbound frame
    /// MUST carry `sandbox_id`; subsequent frames carry only the
    /// `kind` + `data` (and close_code/close_reason when CLOSE).
    /// The server opens a ShellTunnel via `HostClient::proxy_shell`
    /// (which dials ttyd in the right netns), then bridges the gRPC
    /// stream's frames to/from the tunnel's channels.
    async fn proxy_shell(
        &self,
        req: Request<tonic::Streaming<ProxyShellFrame>>,
    ) -> Result<Response<Self::ProxyShellStream>, Status> {
        let mut inbound = req.into_inner();

        // First frame carries sandbox_id. Wait for it (with a small
        // budget so a misbehaving client doesn't pin a handler) and
        // open the tunnel.
        use futures::StreamExt;
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), inbound.next())
            .await
            .map_err(|_| Status::deadline_exceeded("proxy_shell: no first frame within 5s"))?
            .ok_or_else(|| Status::cancelled("proxy_shell: client closed before first frame"))?
            .map_err(|e| Status::internal(format!("proxy_shell: recv first frame: {e}")))?;
        let sandbox_id = decode_sandbox_id(&first.sandbox_id)?;

        let tunnel = self
            .inner
            .proxy_shell(sandbox_id)
            .await
            .map_err(sandbox_to_status)?;
        let engram_core::types::shell::ShellTunnel {
            outbound: tunnel_outbound,
            inbound: mut tunnel_inbound,
        } = tunnel;

        // mpsc carrying frames out to the gRPC client (browser side).
        let (out_tx, out_rx) = mpsc::channel::<Result<ProxyShellFrame, Status>>(64);

        // gRPC inbound (client → us) drains `inbound`; we forward
        // each frame into the ShellTunnel's outbound channel (which
        // the tunnel pump then writes to ttyd). The first frame
        // we already consumed above — if it carried data (it
        // shouldn't, but defensively forward it).
        let first_payload = grpc_frame_to_shell_frame(first);
        if let Some(sf) = first_payload {
            let _ = tunnel_outbound.send(sf).await;
        }
        let tunnel_outbound_for_pump = tunnel_outbound.clone();
        tokio::spawn(async move {
            while let Some(next) = inbound.next().await {
                let frame = match next {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::warn!(error = %e, "proxy_shell: client recv error");
                        break;
                    }
                };
                let Some(sf) = grpc_frame_to_shell_frame(frame) else {
                    continue;
                };
                if tunnel_outbound_for_pump.send(sf).await.is_err() {
                    break;
                }
            }
            // Client closed; drop the tunnel outbound so the
            // tunnel's pump tears down.
            drop(tunnel_outbound_for_pump);
        });

        // Tunnel inbound (ttyd → us) drains here; we forward each
        // frame to the gRPC client out_tx.
        tokio::spawn(async move {
            while let Some(sf) = tunnel_inbound.recv().await {
                let frame = shell_frame_to_grpc_frame(sf);
                if out_tx.send(Ok(frame)).await.is_err() {
                    break;
                }
            }
        });

        let out_stream = tokio_stream::wrappers::ReceiverStream::new(out_rx);
        Ok(Response::new(Box::pin(out_stream) as Self::ProxyShellStream))
    }
}

/// Translate a proto `ProxyShellFrame` into the trait-level
/// [`engram_core::types::shell::ShellFrame`]. `None` for frames that
/// carry no payload (e.g. control frames the server should ignore).
fn grpc_frame_to_shell_frame(
    frame: ProxyShellFrame,
) -> Option<engram_core::types::shell::ShellFrame> {
    use engram_core::types::shell::{ShellClose, ShellFrame};
    let kind = ProxyShellKind::try_from(frame.kind).ok()?;
    Some(match kind {
        ProxyShellKind::Text => ShellFrame::Text(String::from_utf8_lossy(&frame.data).into_owned()),
        ProxyShellKind::Binary => ShellFrame::Binary(frame.data.into()),
        ProxyShellKind::Ping => ShellFrame::Ping(frame.data.into()),
        ProxyShellKind::Pong => ShellFrame::Pong(frame.data.into()),
        ProxyShellKind::Close => {
            if frame.close_code == 0 && frame.close_reason.is_empty() {
                ShellFrame::Close(None)
            } else {
                ShellFrame::Close(Some(ShellClose {
                    code: frame.close_code as u16,
                    reason: frame.close_reason,
                }))
            }
        }
    })
}

/// Translate a [`engram_core::types::shell::ShellFrame`] into a proto
/// `ProxyShellFrame`. Used by both the host-agent server (ttyd →
/// client) and the gRPC client (client → ttyd).
fn shell_frame_to_grpc_frame(frame: engram_core::types::shell::ShellFrame) -> ProxyShellFrame {
    use engram_core::types::shell::ShellFrame;
    let (kind, data, close_code, close_reason) = match frame {
        ShellFrame::Text(t) => (ProxyShellKind::Text, t.into_bytes(), 0, String::new()),
        ShellFrame::Binary(b) => (ProxyShellKind::Binary, b.to_vec(), 0, String::new()),
        ShellFrame::Ping(b) => (ProxyShellKind::Ping, b.to_vec(), 0, String::new()),
        ShellFrame::Pong(b) => (ProxyShellKind::Pong, b.to_vec(), 0, String::new()),
        ShellFrame::Close(None) => (ProxyShellKind::Close, Vec::new(), 0, String::new()),
        ShellFrame::Close(Some(c)) => (ProxyShellKind::Close, Vec::new(), c.code as u32, c.reason),
    };
    ProxyShellFrame {
        sandbox_id: Vec::new(),
        kind: kind as i32,
        data,
        close_code,
        close_reason,
    }
}

// ---- helpers ----

fn encode_bincode<T: serde::Serialize>(value: &T, kind: &'static str) -> Result<Vec<u8>, Status> {
    bincode::serialize(value).map_err(|e| Status::internal(format!("bincode encode {kind}: {e}")))
}

fn decode_bincode<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    kind: &'static str,
) -> Result<T, Status> {
    bincode::deserialize(bytes)
        .map_err(|e| Status::invalid_argument(format!("bincode decode {kind}: {e}")))
}

fn decode_sandbox_id(bytes: &[u8]) -> Result<engram_core::SandboxId, Status> {
    if bytes.len() != 16 {
        return Err(Status::invalid_argument(format!(
            "sandbox_id must be 16 bytes, got {}",
            bytes.len()
        )));
    }
    let mut buf = [0u8; 16];
    buf.copy_from_slice(bytes);
    Ok(engram_core::SandboxId(uuid::Uuid::from_bytes(buf)))
}

fn decode_template_ref(bytes: &[u8]) -> Result<engram_core::types::ids::TemplateRef, Status> {
    if bytes.len() != 16 {
        return Err(Status::invalid_argument(format!(
            "template_ref must be 16 bytes, got {}",
            bytes.len()
        )));
    }
    let mut buf = [0u8; 16];
    buf.copy_from_slice(bytes);
    Ok(engram_core::types::ids::TemplateRef::from(
        uuid::Uuid::from_bytes(buf),
    ))
}

fn decode_session_id(bytes: &[u8]) -> Result<engram_core::SessionId, Status> {
    if bytes.len() != 16 {
        return Err(Status::invalid_argument(format!(
            "session_id must be 16 bytes, got {}",
            bytes.len()
        )));
    }
    let mut buf = [0u8; 16];
    buf.copy_from_slice(bytes);
    Ok(engram_core::SessionId(uuid::Uuid::from_bytes(buf)))
}

fn sandbox_to_status(err: SandboxError) -> Status {
    match err {
        SandboxError::NotFound => Status::not_found(err.to_string()),
        SandboxError::AlreadyExists => Status::already_exists(err.to_string()),
        SandboxError::LimitExceeded(_) => Status::resource_exhausted(err.to_string()),
        SandboxError::InvalidSpec(_) => Status::invalid_argument(err.to_string()),
        SandboxError::Timeout => Status::deadline_exceeded(err.to_string()),
        SandboxError::Snapshot(_) | SandboxError::Io(_) | SandboxError::Vm(_) => {
            Status::internal(err.to_string())
        }
    }
}
