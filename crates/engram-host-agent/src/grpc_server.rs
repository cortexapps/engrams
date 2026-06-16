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
use engram_protocol::grpc::proxy_shell_message::Body as ProxyShellBody;
use engram_protocol::grpc::{
    ApplyEgressPolicyRequest, BindHarnessSessionRequest, BuildBaseSnapshotRequest,
    BuildBaseSnapshotResponse, CowStateAllResponse, CowStateResponse, CreateSandboxRequest,
    CreateSandboxResponse, DrainOutcomeResponse, Empty, ExecExit, ExecFrame, ExecStartRequest,
    GuestIpResponse, InterruptHarnessRequest, ListSandboxesResponse, MigrationCaptureResponse,
    MigrationExportRef, MigrationFetchRequest, MigrationFrame, MigrationPresetupResponse,
    PostCopyCaptureResponse, ProxyShellBinary, ProxyShellClose, ProxyShellMessage, ProxyShellPing,
    ProxyShellPong, ProxyShellText, ReapMaterializeDirRequest, ReapMaterializeDirResponse,
    RestoreBaseForSessionRequest, RestoreRequest, SandboxIdMessage, SendHarnessPromptRequest,
    SnapshotBeginResponse, SnapshotResponse, StartAgentRequest, UnbindHarnessSessionRequest,
};
use engram_protocol::wire::{WireExecRequest, WireReapStats};
use futures::Stream;
use std::pin::Pin;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};
use tracing::Instrument;

/// Link a handler span to the caller's distributed trace (ADR 0019) by
/// reading the W3C `traceparent` the coord's [`TraceparentInjector`] put
/// in the request metadata. No-op when absent (OTLP disabled, or a caller
/// that doesn't propagate).
fn link_remote_parent<T>(span: &tracing::Span, req: &Request<T>) {
    if let Some(tp) = req
        .metadata()
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
    {
        engram_telemetry::set_parent_from_traceparent(span, tp);
    }
}

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
        Pin<Box<dyn Stream<Item = Result<ProxyShellMessage, Status>> + Send + 'static>>;

    async fn ping(&self, _req: Request<Empty>) -> Result<Response<Empty>, Status> {
        Ok(Response::new(Empty {}))
    }

    async fn create_sandbox(
        &self,
        req: Request<CreateSandboxRequest>,
    ) -> Result<Response<CreateSandboxResponse>, Status> {
        let span = tracing::info_span!("host.create_sandbox");
        link_remote_parent(&span, &req);
        // Issue #229: reject a wire_version-skewed coord BEFORE decoding
        // the bincode SandboxSpec (a skew would EOF mid-decode → a 400).
        check_wire_version(&req)?;
        async move {
            let spec = decode_bincode(&req.into_inner().spec_bincode, "SandboxSpec")?;
            let sandbox_id = self.inner.create(spec).await.map_err(sandbox_to_status)?;
            Ok(Response::new(CreateSandboxResponse {
                sandbox_id: sandbox_id.as_uuid().as_bytes().to_vec(),
            }))
        }
        .instrument(span)
        .await
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
        let span = tracing::info_span!("host.snapshot");
        link_remote_parent(&span, &req);
        async move {
            let id = decode_sandbox_id(&req.into_inner().uuid)?;
            let metadata = self.inner.snapshot(id).await.map_err(sandbox_to_status)?;
            Ok(Response::new(SnapshotResponse {
                metadata_bincode: encode_bincode(&metadata, "SnapshotMetadata")?,
            }))
        }
        .instrument(span)
        .await
    }

    async fn snapshot_begin(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<SnapshotBeginResponse>, Status> {
        let span = tracing::info_span!("host.snapshot_begin");
        link_remote_parent(&span, &req);
        async move {
            let id = decode_sandbox_id(&req.into_inner().uuid)?;
            let snapshot_id = self
                .inner
                .snapshot_begin(id)
                .await
                .map_err(sandbox_to_status)?;
            Ok(Response::new(SnapshotBeginResponse {
                snapshot_id: snapshot_id.as_uuid().as_bytes().to_vec(),
            }))
        }
        .instrument(span)
        .await
    }

    async fn snapshot_wait(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<SnapshotResponse>, Status> {
        let span = tracing::info_span!("host.snapshot_wait");
        link_remote_parent(&span, &req);
        async move {
            let id = decode_sandbox_id(&req.into_inner().uuid)?;
            let metadata = self
                .inner
                .snapshot_wait(id)
                .await
                .map_err(sandbox_to_status)?;
            Ok(Response::new(SnapshotResponse {
                metadata_bincode: encode_bincode(&metadata, "SnapshotMetadata")?,
            }))
        }
        .instrument(span)
        .await
    }

    async fn migration_capture(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<MigrationCaptureResponse>, Status> {
        let span = tracing::info_span!("host.migration_capture");
        link_remote_parent(&span, &req);
        async move {
            let id = decode_sandbox_id(&req.into_inner().uuid)?;
            let out = self
                .inner
                .migration_capture(id)
                .await
                .map_err(sandbox_to_status)?;
            Ok(Response::new(MigrationCaptureResponse {
                export_id: out.export_id,
                memory_manifest_json: out.memory_manifest_json,
                disk_manifest_json: out.disk_manifest_json,
                new_memory_chunk_hashes: out
                    .new_memory_chunk_hashes
                    .into_iter()
                    .map(|h| h.to_vec())
                    .collect(),
                new_disk_chunk_hashes: out
                    .new_disk_chunk_hashes
                    .into_iter()
                    .map(|h| h.to_vec())
                    .collect(),
                snapshot_id: out.snapshot_id.as_uuid().as_bytes().to_vec(),
                paused_at_unix_ms: out.paused_at_unix_ms,
                memory_manifest_ref: encode_bincode(&out.memory_manifest_ref, "ManifestRef")?,
                disk_manifest_ref: encode_bincode(&out.disk_manifest_ref, "ManifestRef")?,
                hot_chunks: out.hot_chunks.into_iter().map(|h| h.to_vec()).collect(),
            }))
        }
        .instrument(span)
        .await
    }

    type MigrationFetchStream =
        Pin<Box<dyn Stream<Item = Result<MigrationFrame, Status>> + Send + 'static>>;

    async fn migration_fetch(
        &self,
        req: Request<MigrationFetchRequest>,
    ) -> Result<Response<Self::MigrationFetchStream>, Status> {
        let req = req.into_inner();
        let items: Vec<engram_core::types::snapshot::MigrationItem> = req
            .items
            .into_iter()
            .map(|item| {
                use engram_protocol::grpc::migration_item::Kind;
                match Kind::try_from(item.kind) {
                    Ok(Kind::StateBin) => Ok(engram_core::types::snapshot::MigrationItem::StateBin),
                    Ok(Kind::Sidecar) => Ok(engram_core::types::snapshot::MigrationItem::Sidecar),
                    Ok(Kind::DiskManifest) => {
                        Ok(engram_core::types::snapshot::MigrationItem::DiskManifest)
                    }
                    Ok(Kind::DiskSealInfo) => {
                        Ok(engram_core::types::snapshot::MigrationItem::DiskSealInfo)
                    }
                    Ok(Kind::DiskChunkAt) => Ok(
                        engram_core::types::snapshot::MigrationItem::DiskChunkAt(item.chunk_idx),
                    ),
                    Ok(Kind::Chunk) => {
                        let hash: [u8; 32] =
                            item.hash.as_slice().try_into().map_err(|_| {
                                Status::invalid_argument("chunk hash must be 32 bytes")
                            })?;
                        Ok(engram_core::types::snapshot::MigrationItem::Chunk(hash))
                    }
                    Err(_) => Err(Status::invalid_argument("unknown migration item kind")),
                }
            })
            .collect::<Result<_, Status>>()?;
        let inner_stream = self
            .inner
            .migration_fetch(&req.export_id, items)
            .await
            .map_err(sandbox_to_status)?;
        use futures::StreamExt;
        let mapped = inner_stream.map(|frame| {
            frame
                .map(|f| MigrationFrame {
                    item_idx: f.item_idx,
                    offset: f.offset,
                    data: f.data.to_vec(),
                    last: f.last,
                })
                .map_err(sandbox_to_status)
        });
        Ok(Response::new(Box::pin(mapped)))
    }

    async fn migration_commit(
        &self,
        req: Request<MigrationExportRef>,
    ) -> Result<Response<Empty>, Status> {
        let req = req.into_inner();
        let id = decode_sandbox_id(&req.sandbox_id)?;
        self.inner
            .migration_commit(id, &req.export_id)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn migration_abort(
        &self,
        req: Request<MigrationExportRef>,
    ) -> Result<Response<Empty>, Status> {
        let req = req.into_inner();
        let id = decode_sandbox_id(&req.sandbox_id)?;
        self.inner
            .migration_abort(id, &req.export_id)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn migration_presetup(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<MigrationPresetupResponse>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        let out = self
            .inner
            .migration_presetup(id)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(MigrationPresetupResponse {
            export_id: out.export_id,
            peer_token: out.peer_token,
            peer_port: out.peer_port as u32,
            sidecar_json: out.sidecar_json,
            memory_manifest_json: out.memory_manifest_json,
            memory_manifest_ref: encode_bincode(&out.memory_manifest_ref, "ManifestRef")?,
            disk_manifest_ref: encode_bincode(&out.disk_manifest_ref, "Option<ManifestRef>")?,
            hot_chunks: out.hot_chunks.into_iter().map(|h| h.to_vec()).collect(),
        }))
    }

    async fn migration_capture_post_copy(
        &self,
        req: Request<MigrationExportRef>,
    ) -> Result<Response<PostCopyCaptureResponse>, Status> {
        let req = req.into_inner();
        let id = decode_sandbox_id(&req.sandbox_id)?;
        let out = self
            .inner
            .migration_capture_postcopy(id, &req.export_id)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(PostCopyCaptureResponse {
            sealed_chunks: out.sealed_chunks,
            total_chunks: out.total_chunks,
            scan_ms: out.scan_ms,
            pause_ms: out.pause_ms,
            disk_drain_ms: out.disk_drain_ms,
            vmstate_ms: out.vmstate_ms,
            sealed_disk_chunks: out.sealed_disk_chunks,
            paused_at_unix_ms: out.paused_at_unix_ms,
        }))
    }

    async fn migration_drain_wait(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<DrainOutcomeResponse>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        let out = self
            .inner
            .migration_drain_wait(id)
            .await
            .map_err(sandbox_to_status)?;
        use engram_core::types::snapshot::DrainOutcome;
        Ok(Response::new(match out {
            DrainOutcome::Done {
                pulled,
                alt_sourced,
                zero_chunks,
                ms,
            } => DrainOutcomeResponse {
                done: true,
                pulled,
                alt_sourced,
                zero_chunks,
                ms,
                remaining: 0,
                detail: String::new(),
            },
            DrainOutcome::PeerLost { remaining, detail } => DrainOutcomeResponse {
                done: false,
                pulled: 0,
                alt_sourced: 0,
                zero_chunks: 0,
                ms: 0,
                remaining,
                detail,
            },
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

    // ADR 0045 Phase F: freeze / unfreeze a running microVM in place.
    async fn pause_sandbox(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<Empty>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        self.inner.pause(id).await.map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn resume_sandbox(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<Empty>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        self.inner.resume(id).await.map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn restore(
        &self,
        req: Request<RestoreRequest>,
    ) -> Result<Response<SandboxIdMessage>, Status> {
        let span = tracing::info_span!("host.restore");
        link_remote_parent(&span, &req);
        check_wire_version(&req)?;
        async move {
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
        .instrument(span)
        .await
    }

    async fn build_base_snapshot(
        &self,
        req: Request<BuildBaseSnapshotRequest>,
    ) -> Result<Response<BuildBaseSnapshotResponse>, Status> {
        let span = tracing::info_span!("host.build_base_snapshot");
        link_remote_parent(&span, &req);
        check_wire_version(&req)?;
        async move {
            let inner = req.into_inner();
            let spec = decode_bincode(&inner.spec_bincode, "SandboxSpec")?;
            // `warm_bincode` is the optional `[warm]` hook. A populated
            // buffer is `Option<WarmConfig>` (a `None` still encodes to a
            // 1-byte discriminant); a genuinely empty buffer (an unset
            // proto field) is treated as `None`.
            let warm = if inner.warm_bincode.is_empty() {
                None
            } else {
                decode_bincode(&inner.warm_bincode, "Option<WarmConfig>")?
            };
            let metadata = self
                .inner
                .build_base_snapshot(spec, warm)
                .await
                .map_err(sandbox_to_status)?;
            Ok(Response::new(BuildBaseSnapshotResponse {
                metadata_bincode: encode_bincode(&metadata, "SnapshotMetadata")?,
            }))
        }
        .instrument(span)
        .await
    }

    async fn restore_base_for_session(
        &self,
        req: Request<RestoreBaseForSessionRequest>,
    ) -> Result<Response<SandboxIdMessage>, Status> {
        let span = tracing::info_span!("host.restore_base_for_session");
        link_remote_parent(&span, &req);
        check_wire_version(&req)?;
        async move {
            let inner = req.into_inner();
            let metadata = decode_bincode(&inner.metadata_bincode, "SnapshotMetadata")?;
            let id = self
                .inner
                .restore_base_for_session(metadata, inner.session_env)
                .await
                .map_err(sandbox_to_status)?;
            Ok(Response::new(SandboxIdMessage {
                uuid: id.as_uuid().as_bytes().to_vec(),
            }))
        }
        .instrument(span)
        .await
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

    async fn interrupt_harness(
        &self,
        req: Request<InterruptHarnessRequest>,
    ) -> Result<Response<Empty>, Status> {
        let r = req.into_inner();
        let sandbox_id = decode_sandbox_id(&r.sandbox_id)?;
        self.inner
            .interrupt(sandbox_id)
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
        // `agent_handshake` is the suspected dominant cold-boot phase
        // (in-VM kernel boot + ext4 mount + NBD page-in + agentd dial);
        // this span makes it visible in the end-to-end trace (ADR 0019).
        let span = tracing::info_span!("host.start_agent", phase = "agent_handshake");
        link_remote_parent(&span, &req);
        check_wire_version(&req)?;
        async move {
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
        .instrument(span)
        .await
    }

    async fn apply_egress_policy(
        &self,
        req: Request<ApplyEgressPolicyRequest>,
    ) -> Result<Response<Empty>, Status> {
        check_wire_version(&req)?;
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

    async fn renew_shell(&self, req: Request<SandboxIdMessage>) -> Result<Response<Empty>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        self.inner
            .renew_shell(id)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
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

    /// ADR 0016 Phase A: per-sandbox COW diagnostic. Returns an
    /// empty bincode blob when the host has no chunk-tracked view
    /// of the sandbox — distinguishes "not chunk-tracked" from
    /// "tracked but zero dirty" on the wire so the coord can render
    /// the two states differently.
    async fn cow_state(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<CowStateResponse>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        let state = self.inner.cow_state(id).await.map_err(sandbox_to_status)?;
        let state_bincode = match state {
            Some(s) => encode_bincode(&s, "CowState")?,
            None => Vec::new(),
        };
        Ok(Response::new(CowStateResponse { state_bincode }))
    }

    /// ADR 0016 Phase A: bulk COW fetch. The bincode payload is a
    /// `Vec<CowStateRecord>` — empty vec encodes as a non-empty
    /// blob (the length prefix is 0).
    async fn cow_state_all(
        &self,
        _req: Request<Empty>,
    ) -> Result<Response<CowStateAllResponse>, Status> {
        let records = self
            .inner
            .cow_state_all()
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(CowStateAllResponse {
            records_bincode: encode_bincode(&records, "Vec<CowStateRecord>")?,
        }))
    }

    /// ADR 0016 Phase B commit 4a — admin flush trigger. Returns
    /// `Some(ManifestRef)` (bincode-encoded) if chunks were drained;
    /// empty bytes otherwise.
    async fn flush_sandbox(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<engram_protocol::grpc::FlushSandboxResponse>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        let outcome = self
            .inner
            .flush_sandbox(id)
            .await
            .map_err(sandbox_to_status)?;
        let manifest_ref_bincode = match outcome {
            Some(mref) => encode_bincode(&mref, "ManifestRef")?,
            None => Vec::new(),
        };
        Ok(Response::new(engram_protocol::grpc::FlushSandboxResponse {
            manifest_ref_bincode,
        }))
    }

    /// Server-streaming exec. First frame is `started` (carries the
    /// host-assigned `exec_id`); subsequent frames carry stdout /
    /// stderr bytes; terminal frame is `exit` (always exactly one).
    async fn exec_start(
        &self,
        req: Request<ExecStartRequest>,
    ) -> Result<Response<Self::ExecStartStream>, Status> {
        check_wire_version(&req)?;
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

    /// ADR 0014 issue #6: bidi WS-frame tunnel.
    ///
    /// Wire contract (ADR 0015 follow-up, prod session 8725648d on
    /// 2026-05-23): the FIRST message MUST be a `ProxyShellOpen`
    /// carrying `sandbox_id`. Any other variant as the first message
    /// is `InvalidArgument`. Any subsequent `Open` is also
    /// `InvalidArgument` (the open variant has no meaning past the
    /// stream handshake). All other variants are pure ShellFrames
    /// that we forward into the host-side tunnel. The prior shape
    /// (single `ProxyShellFrame` with `sandbox_id` meaningful only
    /// on the first frame, plus a `kind/data` field-bag) silently
    /// forwarded the sentinel's empty payload as a WS Binary frame
    /// to ttyd; 1.7.8 TCP-RSTs on that. The oneof here makes the bug
    /// impossible to write in this direction.
    async fn proxy_shell(
        &self,
        req: Request<tonic::Streaming<ProxyShellMessage>>,
    ) -> Result<Response<Self::ProxyShellStream>, Status> {
        let mut inbound = req.into_inner();

        // First message must be Open. 5s budget so a misbehaving
        // client doesn't pin a handler.
        use futures::StreamExt;
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), inbound.next())
            .await
            .map_err(|_| Status::deadline_exceeded("proxy_shell: no first message within 5s"))?
            .ok_or_else(|| Status::cancelled("proxy_shell: client closed before first message"))?
            .map_err(|e| Status::internal(format!("proxy_shell: recv first message: {e}")))?;
        let sandbox_id = match first.body {
            Some(ProxyShellBody::Open(open)) => decode_sandbox_id(&open.sandbox_id)?,
            Some(other) => {
                return Err(Status::invalid_argument(format!(
                    "proxy_shell: first message must be Open, got {}",
                    proxy_body_kind_name(&other),
                )));
            }
            None => {
                return Err(Status::invalid_argument(
                    "proxy_shell: first message has empty body",
                ));
            }
        };

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
        let (out_tx, out_rx) = mpsc::channel::<Result<ProxyShellMessage, Status>>(64);

        // gRPC inbound (client → us) drains `inbound`; we forward
        // each frame into the ShellTunnel's outbound channel (which
        // the tunnel pump then writes to ttyd). Open is rejected
        // here too — it's a stream-handshake variant, not data.
        let out_tx_for_open_reject = out_tx.clone();
        tokio::spawn(async move {
            while let Some(next) = inbound.next().await {
                let msg = match next {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(error = %e, "proxy_shell: client recv error");
                        break;
                    }
                };
                let sf = match proxy_body_to_shell_frame(msg.body) {
                    Ok(Some(sf)) => sf,
                    Ok(None) => continue, // body=None — ignore
                    Err(e) => {
                        // Open-after-handshake (or any other illegal
                        // shape); tear down the stream with a typed
                        // status so the client surfaces a clean
                        // error instead of an opaque close.
                        let _ = out_tx_for_open_reject
                            .send(Err(Status::invalid_argument(format!("proxy_shell: {e}"))))
                            .await;
                        break;
                    }
                };
                if tunnel_outbound.send(sf).await.is_err() {
                    break;
                }
            }
            // Client closed; drop the tunnel outbound so the
            // tunnel's pump tears down.
            drop(tunnel_outbound);
        });

        // Tunnel inbound (ttyd → us) drains here; we forward each
        // frame to the gRPC client out_tx.
        tokio::spawn(async move {
            while let Some(sf) = tunnel_inbound.recv().await {
                let msg = ProxyShellMessage {
                    body: Some(shell_frame_to_proxy_body(sf)),
                };
                if out_tx.send(Ok(msg)).await.is_err() {
                    break;
                }
            }
        });

        let out_stream = tokio_stream::wrappers::ReceiverStream::new(out_rx);
        Ok(Response::new(Box::pin(out_stream) as Self::ProxyShellStream))
    }
}

/// Translate a server-bound `ProxyShellBody` into a [`ShellFrame`].
///
/// - `Ok(Some(frame))` for the data variants (Text/Binary/Ping/Pong/Close).
/// - `Ok(None)` for `body == None` — the prost wire permits an
///   empty oneof; we silently drop those rather than tear down the
///   shell.
/// - `Err(_)` for `Open` after the handshake. That's a protocol
///   violation: Open is a one-shot stream-open variant, and we
///   already consumed the legitimate first Open before reaching
///   this codepath. The caller surfaces this as
///   `InvalidArgument` on the gRPC stream.
fn proxy_body_to_shell_frame(
    body: Option<ProxyShellBody>,
) -> Result<Option<engram_core::types::shell::ShellFrame>, String> {
    use engram_core::types::shell::{ShellClose, ShellFrame};
    Ok(match body {
        None => None,
        Some(ProxyShellBody::Open(_)) => {
            return Err("Open is only legal as the first message".into());
        }
        Some(ProxyShellBody::Text(t)) => Some(ShellFrame::Text(t.data)),
        Some(ProxyShellBody::Binary(b)) => Some(ShellFrame::Binary(b.data.into())),
        Some(ProxyShellBody::Ping(p)) => Some(ShellFrame::Ping(p.data.into())),
        Some(ProxyShellBody::Pong(p)) => Some(ShellFrame::Pong(p.data.into())),
        Some(ProxyShellBody::Close(c)) => {
            if c.code == 0 && c.reason.is_empty() {
                Some(ShellFrame::Close(None))
            } else {
                Some(ShellFrame::Close(Some(ShellClose {
                    code: c.code as u16,
                    reason: c.reason,
                })))
            }
        }
    })
}

/// Translate a [`ShellFrame`] into the corresponding `ProxyShellBody`
/// data variant. Total — every ShellFrame maps 1:1 to one of the
/// data variants — so this never produces an `Open`.
fn shell_frame_to_proxy_body(frame: engram_core::types::shell::ShellFrame) -> ProxyShellBody {
    use engram_core::types::shell::ShellFrame;
    match frame {
        ShellFrame::Text(t) => ProxyShellBody::Text(ProxyShellText { data: t }),
        ShellFrame::Binary(b) => ProxyShellBody::Binary(ProxyShellBinary { data: b.to_vec() }),
        ShellFrame::Ping(b) => ProxyShellBody::Ping(ProxyShellPing { data: b.to_vec() }),
        ShellFrame::Pong(b) => ProxyShellBody::Pong(ProxyShellPong { data: b.to_vec() }),
        ShellFrame::Close(None) => ProxyShellBody::Close(ProxyShellClose {
            code: 0,
            reason: String::new(),
        }),
        ShellFrame::Close(Some(c)) => ProxyShellBody::Close(ProxyShellClose {
            code: c.code as u32,
            reason: c.reason,
        }),
    }
}

/// Render a `ProxyShellBody` variant name for diagnostics — used by
/// the "first message must be Open" error to surface what was
/// actually received without dumping the payload.
fn proxy_body_kind_name(body: &ProxyShellBody) -> &'static str {
    match body {
        ProxyShellBody::Open(_) => "Open",
        ProxyShellBody::Text(_) => "Text",
        ProxyShellBody::Binary(_) => "Binary",
        ProxyShellBody::Ping(_) => "Ping",
        ProxyShellBody::Pong(_) => "Pong",
        ProxyShellBody::Close(_) => "Close",
    }
}

// ---- helpers ----

fn encode_bincode<T: serde::Serialize>(value: &T, kind: &'static str) -> Result<Vec<u8>, Status> {
    bincode::serialize(value).map_err(|e| Status::internal(format!("bincode encode {kind}: {e}")))
}

/// Issue #229: reject a coord→host RPC whose stamped wire version differs
/// from ours BEFORE any bincode decode. The coordinator's
/// `TraceparentInjector` puts its `WIRE_VERSION` in the
/// [`engram_protocol::wire::WIRE_VERSION_METADATA_KEY`] metadata header
/// on every request; a mismatch means a mixed-version fleet mid rolling
/// deploy, where a bincode decode would otherwise EOF and surface to the
/// user as a misleading 400 "invalid sandbox spec". We return
/// `failed_precondition` with the skew marker so the coord maps it to a
/// retryable 503 (`SandboxError::WireSkew`).
///
/// A request with NO version header (a coordinator that predates this
/// field) is tolerated — we can't compare what we didn't receive, and
/// the decode path is the same as before this fix. A header we can't
/// parse is likewise tolerated (fail-open) rather than blocking traffic
/// on a malformed value.
fn check_wire_version<T>(req: &Request<T>) -> Result<(), Status> {
    let Some(raw) = req
        .metadata()
        .get(engram_protocol::wire::WIRE_VERSION_METADATA_KEY)
    else {
        return Ok(());
    };
    let Some(coord) = raw.to_str().ok().and_then(|s| s.parse::<u32>().ok()) else {
        return Ok(());
    };
    let host = engram_protocol::WIRE_VERSION;
    if coord == host {
        return Ok(());
    }
    ::metrics::counter!("engram_host_wire_skew_rejections_total").increment(1);
    tracing::error!(
        host_wire_version = host,
        coord_wire_version = coord,
        "rejecting coord RPC: wire_version skew (mixed-version fleet during a rolling \
         deploy); coord retries onto a version-matched host",
    );
    Err(Status::failed_precondition(
        engram_protocol::wire::wire_skew_message(host, coord),
    ))
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
        SandboxError::ImageNotReady(_) => Status::failed_precondition(err.to_string()),
        // Host-agent never originates HostLost — that's a coord-side
        // signal (ADR 0015 M3). Map defensively in case a future
        // refactor surfaces it here.
        SandboxError::HostLost => Status::failed_precondition(err.to_string()),
        // ADR 0050 C: a transient inner failure round-trips back as
        // Unavailable so the coord's retry logic keys on it uniformly.
        SandboxError::Unavailable(_) => Status::unavailable(err.to_string()),
        // Issue #229: the host-agent never ORIGINATES WireSkew from its
        // local backend (it's the coord-side decode of our own
        // `failed_precondition` skew status). Map defensively in case a
        // future refactor surfaces it here.
        SandboxError::WireSkew { host, coord } => {
            Status::failed_precondition(engram_protocol::wire::wire_skew_message(host, coord))
        }
    }
}

#[cfg(test)]
mod wire_version_tests {
    use super::*;
    use engram_protocol::wire::WIRE_VERSION_METADATA_KEY;

    /// Build a `Request<()>` carrying an `x-engram-wire-version` metadata
    /// header set to `coord` — the shape the coord's `TraceparentInjector`
    /// produces. `None` simulates a coordinator that predates the field.
    fn req_with_version(coord: Option<u32>) -> Request<()> {
        let mut req = Request::new(());
        if let Some(v) = coord {
            req.metadata_mut()
                .insert(WIRE_VERSION_METADATA_KEY, v.to_string().parse().unwrap());
        }
        req
    }

    #[test]
    fn matching_wire_version_is_accepted() {
        let req = req_with_version(Some(engram_protocol::WIRE_VERSION));
        assert!(check_wire_version(&req).is_ok());
    }

    #[test]
    fn skewed_wire_version_is_rejected_with_failed_precondition_marker() {
        // Issue #229: a skewed coord must be refused at the RPC boundary
        // BEFORE any bincode decode — so the caller sees an explicit,
        // retryable skew (`failed_precondition` + marker → 503), not the
        // 400 "invalid sandbox spec" a mid-decode EOF would have produced.
        let coord = engram_protocol::WIRE_VERSION + 1;
        let req = req_with_version(Some(coord));
        let status = check_wire_version(&req).expect_err("skew must be rejected");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            engram_protocol::wire::parse_wire_skew_message(status.message()),
            Some((engram_protocol::WIRE_VERSION, coord)),
        );
    }

    #[test]
    fn absent_version_header_is_tolerated() {
        // A coordinator that predates the wire-version header (mid-roll)
        // must not be blocked — we can't compare what we didn't receive.
        let req = req_with_version(None);
        assert!(check_wire_version(&req).is_ok());
    }

    #[test]
    fn unparseable_version_header_fails_open() {
        // A malformed header value must not wedge traffic — fail open.
        let mut req = Request::new(());
        req.metadata_mut()
            .insert(WIRE_VERSION_METADATA_KEY, "not-a-number".parse().unwrap());
        assert!(check_wire_version(&req).is_ok());
    }
}
