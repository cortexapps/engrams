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

use engram_core::traits::{HostClient, SessionFence};
use engram_core::types::sandbox::{ExecEvent, ExecEventStream, SessionFileSpec};
use engram_core::SandboxError;
use engram_protocol::grpc::host_service_server::{HostService, HostServiceServer};
use engram_protocol::grpc::proxy_port_message::Body as ProxyPortBody;
use engram_protocol::grpc::proxy_shell_message::Body as ProxyShellBody;
use engram_protocol::grpc::{
    BindHarnessSessionRequest, BrowserPortResponse, CancelExecRequest, CreateSandboxRequest,
    CreateSandboxResponse, DequeueHarnessQueuedPromptRequest, DrainOutcomeResponse,
    EditHarnessQueuedPromptRequest, Empty, ExecExit, ExecFrame, ExecStartRequest,
    FencedSandboxRequest, GuestIpResponse, HostReadFileMetadata, HostReadFileRequest,
    HostReadFileResponse, HostWriteFileRequest, HostWriteFileResponse, IdePortResponse,
    InterruptHarnessRequest, ListSandboxesResponse, MaterializeImageDone, MaterializeImageEvent,
    MaterializeImageFailed, MaterializeImageRequest, MaterializeProgress,
    MigrationAbortUnservedResponse, MigrationExportRef, MigrationFetchRequest, MigrationFrame,
    MigrationPresetupResponse, PeerChunkFrame, PeerChunkGetRequest, PostCopyCaptureResponse,
    ProbeSandboxResponse, ProxyPortData, ProxyPortMessage, ProxyShellBinary, ProxyShellClose,
    ProxyShellMessage, ProxyShellPing, ProxyShellPong, ProxyShellText,
    RestoreBaseForSessionRequest, RestoreRequest, SandboxIdMessage, SendHarnessPromptRequest,
    SendHarnessToolResultRequest, SnapshotBeginResponse, SnapshotResponse, StartAgentRequest,
    UnbindHarnessSessionRequest,
};
use engram_protocol::grpc_client::link_remote_parent;
use engram_protocol::wire::WireExecRequest;
use futures::Stream;
use std::pin::Pin;
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};
use tracing::Instrument;

use crate::session_epochs::{EpochError, SessionEpochStore};

/// Concrete `HostService` impl. Holds the in-proc
/// `Arc<dyn HostClient>` (typically a `LocalHostClient` wrapping
/// the host's SandboxBackend + HarnessHub).
pub struct HostServiceImpl {
    inner: Arc<dyn HostClient>,
    managed_tls: bool,
    /// ADR 0079: per-session fencing-epoch high-water, durable under
    /// work_dir. Gates every session-scoped lifecycle RPC.
    epochs: SessionEpochStore,
    /// ADR 0095: the standing peer-chunk serve state (cache + readiness
    /// view + stream semaphore). `None` on cache-less hosts (Process
    /// backend), which answer `unavailable` — host infrastructure, not
    /// sandbox lifecycle, so it lives here rather than on `HostClient`.
    peer: Option<Arc<crate::peer_fill::PeerServe>>,
}

impl HostServiceImpl {
    pub fn new(inner: Arc<dyn HostClient>, epochs: SessionEpochStore) -> Self {
        Self {
            inner,
            managed_tls: false,
            epochs,
            peer: None,
        }
    }

    fn check_managed_protocol<T>(&self, request: &Request<T>) -> Result<(), Status> {
        check_wire_version(request)?;
        let exact = |key, expected: u32| {
            request.metadata().get(key).and_then(|v| v.to_str().ok())
                == Some(expected.to_string().as_str())
        };
        if !self.managed_tls
            || !exact(
                engram_protocol::wire::WIRE_VERSION_METADATA_KEY,
                engram_protocol::WIRE_VERSION,
            )
            || !exact(
                engram_protocol::wire::MANAGED_PROTOCOL_METADATA_KEY,
                engram_protocol::wire::MANAGED_PROTOCOL_VERSION,
            )
        {
            return Err(Status::failed_precondition(
                "managed RPC requires authenticated host mode and exact protocol",
            ));
        }
        Ok(())
    }

    pub fn with_peer_serve(mut self, peer: Arc<crate::peer_fill::PeerServe>) -> Self {
        self.peer = Some(peer);
        self
    }

    /// Check the session fence before a lifecycle operation reaches the backend.
    /// Rejections carry the stored and incoming epochs as `failed_precondition`.
    ///
    /// Returns the decoded [`SessionFence`] so the handler can thread it
    /// to the inner `HostClient` without re-decoding.
    /// Epoch zero preserves a supplied identity without accessing the
    /// high-water store. Empty bytes retain the unfenced sentinel.
    ///
    /// `fencing_epoch == 0` (allow WITHOUT advancing the high-water; the
    /// store's `check_and_advance` holds the reject-zero semantics for
    /// whenever this arm is removed) — the coordinator's REMAINING
    /// out-of-op callers, all deliberate (#543 pass 2 disposition):
    ///
    /// - the direct (capacity-available) create pipeline
    ///   (`session_boot::boot_on_reserved_host` via `create_session_core`
    ///   — restore/start_agent/teardown-destroy); only the QUEUED-create
    ///   path rides the create_boot op. Sound: the session is brand new,
    ///   no competing executor can exist before its first op claim.
    /// - the rung-1/2 ascent from the wire (`ensure_active` →
    ///   `try_cancel_nominated_eviction` → `ascend_evicting_to_active`),
    ///   which un-pauses only after proving no op is running.
    /// - the admin in-place `pause`/`resume` debug endpoints
    ///   (`api/admin.rs`) — operator tools, no lifecycle writes.
    ///
    /// TODO(#543): flip 0 to reject once those paths migrate onto ops /
    /// fenced enqueues (the ADR 0079 divergence log records the deferral).
    fn check_session_epoch(
        &self,
        session_id: &[u8],
        fencing_epoch: u64,
    ) -> Result<SessionFence, Status> {
        if fencing_epoch == 0 && session_id.is_empty() {
            return Ok(SessionFence::unfenced());
        }
        let session_id = decode_session_id(session_id)?;
        if fencing_epoch == 0 {
            return Ok(SessionFence::new(session_id, 0));
        }
        self.epochs
            .check_and_advance(session_id, fencing_epoch)
            .map_err(|e| match e {
                EpochError::Stale { stored, incoming } => {
                    ::metrics::counter!("engram_host_fencing_epoch_rejections_total").increment(1);
                    tracing::warn!(
                        %session_id,
                        stored,
                        incoming,
                        "rejecting session-scoped RPC: stale fencing epoch (a successor \
                         executor already re-claimed this session)",
                    );
                    Status::failed_precondition(format!(
                        "fencing epoch stale for session {session_id}: stored high-water \
                         {stored}, incoming {incoming}"
                    ))
                }
                EpochError::Zero => Status::failed_precondition(format!(
                    "fencing epoch 0 is never valid (session {session_id})"
                )),
                EpochError::Io(err) => {
                    Status::internal(format!("session epoch store for {session_id}: {err}"))
                }
            })?;
        Ok(SessionFence::new(session_id, fencing_epoch))
    }
}

/// Bind the `HostService` listener. Connections queue in the kernel
/// until [`serve`] runs, so a self-connect probe passes from here on.
pub fn bind(
    listen_addr: std::net::SocketAddr,
) -> std::io::Result<tonic::transport::server::TcpIncoming> {
    let incoming = tonic::transport::server::TcpIncoming::bind(listen_addr)?;
    tracing::info!(addr = %listen_addr, "gRPC HostService listening");
    Ok(incoming.with_nodelay(Some(true)))
}

/// Bind `listen_addr` and serve the `HostService` until the caller drops
/// the returned future.
pub async fn boot(
    listen_addr: std::net::SocketAddr,
    inner: Arc<dyn HostClient>,
    epochs: SessionEpochStore,
    peer: Option<Arc<crate::peer_fill::PeerServe>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let incoming = bind(listen_addr)?;
    Ok(serve(incoming, inner, epochs, peer).await?)
}

/// Serve the `HostService` on a bound listener until the caller drops
/// the returned future. Ends with `Err` on a server error; a clean
/// Ctrl-C just drops the future, which the runtime collects.
pub async fn serve(
    incoming: tonic::transport::server::TcpIncoming,
    inner: Arc<dyn HostClient>,
    epochs: SessionEpochStore,
    peer: Option<Arc<crate::peer_fill::PeerServe>>,
) -> Result<(), tonic::transport::Error> {
    serve_inner(incoming, inner, epochs, peer, None).await
}

/// Serve one authenticated listener for all lifecycle operations in managed mode.
pub async fn boot_with_managed_tls(
    listen_addr: std::net::SocketAddr,
    inner: Arc<dyn HostClient>,
    epochs: SessionEpochStore,
    peer: Option<Arc<crate::peer_fill::PeerServe>>,
    tls: Option<crate::managed_tls::ManagedHostTls>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let incoming = bind(listen_addr)?;
    serve_with_managed_tls(incoming, inner, epochs, peer, tls).await
}

/// Serve the bound lifecycle listener with optional managed authentication.
pub async fn serve_with_managed_tls(
    incoming: tonic::transport::server::TcpIncoming,
    inner: Arc<dyn HostClient>,
    epochs: SessionEpochStore,
    peer: Option<Arc<crate::peer_fill::PeerServe>>,
    tls: Option<crate::managed_tls::ManagedHostTls>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let tls = tls
        .map(|config| {
            let server = config.server_config().map_err(std::io::Error::other)?;
            Ok::<_, std::io::Error>((server, config.coordinator_pins))
        })
        .transpose()?;
    Ok(serve_inner(incoming, inner, epochs, peer, tls).await?)
}

async fn serve_inner(
    incoming: tonic::transport::server::TcpIncoming,
    inner: Arc<dyn HostClient>,
    epochs: SessionEpochStore,
    peer: Option<Arc<crate::peer_fill::PeerServe>>,
    tls: Option<(
        tonic::transport::ServerTlsConfig,
        std::collections::BTreeSet<String>,
    )>,
) -> Result<(), tonic::transport::Error> {
    let mut svc = HostServiceImpl::new(inner, epochs);
    if let Some(p) = peer {
        svc = svc.with_peer_serve(p);
    }
    svc.managed_tls = tls.is_some();
    let (server, pins) = match tls {
        Some((config, pins)) => (
            tonic::transport::Server::builder().tls_config(config)?,
            Some(pins),
        ),
        None => (tonic::transport::Server::builder(), None),
    };
    let service = HostServiceServer::new(svc)
        .max_encoding_message_size(32 * 1024 * 1024)
        .max_decoding_message_size(9 * 1024 * 1024);
    let authenticated =
        tonic::service::interceptor::InterceptedService::new(service, move |req: Request<()>| {
            if let Some(pins) = &pins {
                let certificates = req
                    .peer_certs()
                    .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
                let leaf = certificates
                    .first()
                    .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
                if !pins.contains(&engram_protocol::host_tls::certificate_pin(leaf.as_ref())) {
                    return Err(Status::unauthenticated("client certificate not authorized"));
                }
                let version = req
                    .metadata()
                    .get(engram_protocol::wire::WIRE_VERSION_METADATA_KEY)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u32>().ok());
                if version != Some(engram_protocol::WIRE_VERSION) {
                    return Err(Status::failed_precondition(
                        "host protocol version required",
                    ));
                }
            }
            Ok(req)
        });
    server
        // ADR 0013: HTTP/2 multiplexes many concurrent streams over
        // one TCP connection per coord pod. 256 is well above the
        // running_sandboxes ceiling — we don't expect to hit it.
        .concurrency_limit_per_connection(256)
        // ADR 0095: peer-chunk pulls move MiB-scale frames between
        // hosts; tonic's default 64 KiB stream / 1 MiB connection
        // windows cap a stream at ~20 MB/s (the teleport transport's
        // measured artifact). Large static windows + adaptive flow
        // control lift the serve side to NIC/NVMe rate; coord-side
        // control RPCs are unaffected (windows are ceilings).
        .initial_stream_window_size(Some(16 * 1024 * 1024))
        .initial_connection_window_size(Some(32 * 1024 * 1024))
        .http2_adaptive_window(Some(true))
        .add_service(authenticated)
        .serve_with_incoming(incoming)
        .await
}

/// Forward one backend exec stream onto the host-service gRPC stream.
///
/// [`ExecEvent::Exit`] and [`ExecEvent::Refused`] are terminal. If the backend
/// event channel closes first, the durable guest journal may still hold the
/// result, so surface `Unavailable` instead of inventing `Exit(None)`.
async fn pump_exec_frames(
    exec_id: String,
    mut events: ExecEventStream,
    tx: mpsc::Sender<Result<ExecFrame, Status>>,
) {
    use futures::StreamExt;

    while let Some(event) = events.next().await {
        let frame = match event {
            ExecEvent::Stdout(bytes) => ExecFrame {
                frame: Some(engram_protocol::grpc::exec_frame::Frame::Stdout(
                    bytes.to_vec(),
                )),
            },
            ExecEvent::Stderr(bytes) => ExecFrame {
                frame: Some(engram_protocol::grpc::exec_frame::Frame::Stderr(
                    bytes.to_vec(),
                )),
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
            ExecEvent::Refused(reason) => {
                let frame = ExecFrame {
                    frame: Some(engram_protocol::grpc::exec_frame::Frame::Refused(reason)),
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

    let _ = tx
        .send(Err(Status::unavailable(format!(
            "exec {exec_id} backend stream ended without an Exit or Refused frame; \
             its result may be recoverable by re-attaching with the same exec_id"
        ))))
        .await;
}

#[tonic::async_trait]
impl HostService for HostServiceImpl {
    type ExecStartStream = Pin<Box<dyn Stream<Item = Result<ExecFrame, Status>> + Send + 'static>>;
    type ProxyShellStream =
        Pin<Box<dyn Stream<Item = Result<ProxyShellMessage, Status>> + Send + 'static>>;
    type ProxyPortStream =
        Pin<Box<dyn Stream<Item = Result<ProxyPortMessage, Status>> + Send + 'static>>;
    type MaterializeImageStream =
        Pin<Box<dyn Stream<Item = Result<MaterializeImageEvent, Status>> + Send + 'static>>;
    type ReadFileStream =
        Pin<Box<dyn Stream<Item = Result<HostReadFileResponse, Status>> + Send + 'static>>;

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
        req: Request<FencedSandboxRequest>,
    ) -> Result<Response<Empty>, Status> {
        check_wire_version(&req)?;
        let r = req.into_inner();
        let fence = self.check_session_epoch(&r.session_id, r.fencing_epoch)?;
        let id = decode_sandbox_id(&r.uuid)?;
        self.inner
            .destroy(id, fence)
            .await
            .map_err(sandbox_to_status)?;
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

    async fn probe_sandbox(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<ProbeSandboxResponse>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        let probe = self
            .inner
            .probe_sandbox(id)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(ProbeSandboxResponse {
            known_to_backend: probe.known_to_backend,
            process_alive: probe.process_alive,
        }))
    }

    async fn snapshot(
        &self,
        req: Request<FencedSandboxRequest>,
    ) -> Result<Response<SnapshotResponse>, Status> {
        let span = tracing::info_span!("host.snapshot");
        link_remote_parent(&span, &req);
        check_wire_version(&req)?;
        async move {
            let r = req.into_inner();
            let fence = self.check_session_epoch(&r.session_id, r.fencing_epoch)?;
            let id = decode_sandbox_id(&r.uuid)?;
            let metadata = self
                .inner
                .snapshot(id, fence)
                .await
                .map_err(sandbox_to_status)?;
            Ok(Response::new(SnapshotResponse {
                metadata_bincode: encode_bincode(&metadata, "SnapshotMetadata")?,
            }))
        }
        .instrument(span)
        .await
    }

    async fn snapshot_hold(
        &self,
        req: Request<FencedSandboxRequest>,
    ) -> Result<Response<SnapshotResponse>, Status> {
        let span = tracing::info_span!("host.snapshot_hold");
        link_remote_parent(&span, &req);
        check_wire_version(&req)?;
        async move {
            let r = req.into_inner();
            let fence = self.check_session_epoch(&r.session_id, r.fencing_epoch)?;
            let id = decode_sandbox_id(&r.uuid)?;
            let metadata = self
                .inner
                .snapshot_hold(id, fence)
                .await
                .map_err(sandbox_to_status)?;
            Ok(Response::new(SnapshotResponse {
                metadata_bincode: encode_bincode(&metadata, "SnapshotMetadata")?,
            }))
        }
        .instrument(span)
        .await
    }

    async fn snapshot_begin(
        &self,
        req: Request<FencedSandboxRequest>,
    ) -> Result<Response<SnapshotBeginResponse>, Status> {
        let span = tracing::info_span!("host.snapshot_begin");
        link_remote_parent(&span, &req);
        check_wire_version(&req)?;
        async move {
            let r = req.into_inner();
            let fence = self.check_session_epoch(&r.session_id, r.fencing_epoch)?;
            let id = decode_sandbox_id(&r.uuid)?;
            let snapshot_id = self
                .inner
                .snapshot_begin(id, fence)
                .await
                .map_err(sandbox_to_status)?;
            Ok(Response::new(SnapshotBeginResponse {
                snapshot_id: snapshot_id.as_uuid().as_bytes().to_vec(),
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
        check_wire_version(&req)?;
        let req = req.into_inner();
        let items: Vec<engram_core::types::snapshot::MigrationItem> = req
            .items
            .into_iter()
            .map(|item| {
                use engram_protocol::grpc::migration_item::Kind;
                let role = match engram_protocol::grpc::DiskRole::try_from(item.role) {
                    Ok(
                        engram_protocol::grpc::DiskRole::Unspecified
                        | engram_protocol::grpc::DiskRole::Root,
                    ) => engram_core::types::snapshot::DiskRole::Root,
                    Ok(engram_protocol::grpc::DiskRole::Swap) => {
                        engram_core::types::snapshot::DiskRole::Swap
                    }
                    Err(_) => return Err(Status::invalid_argument("unknown disk role")),
                };
                match Kind::try_from(item.kind) {
                    Ok(Kind::HotHint) => Ok(engram_core::types::snapshot::MigrationItem::HotHint),
                    Ok(Kind::StateBin) => Ok(engram_core::types::snapshot::MigrationItem::StateBin),
                    Ok(Kind::DiskSealInfo) => Ok(
                        engram_core::types::snapshot::MigrationItem::DiskSealInfo(role),
                    ),
                    Ok(Kind::DiskPrecopyInfo) => {
                        Ok(engram_core::types::snapshot::MigrationItem::DiskPrecopyInfo(role))
                    }
                    Ok(Kind::DiskBlockAt) => Ok(
                        engram_core::types::snapshot::MigrationItem::DiskBlockAt(role, item.index),
                    ),
                    Ok(Kind::Unspecified) | Err(_) => {
                        Err(Status::invalid_argument("unknown migration item kind"))
                    }
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
                    item_hash: f.item_hash.map(|h| h.to_vec()).unwrap_or_default(),
                })
                .map_err(sandbox_to_status)
        });
        Ok(Response::new(Box::pin(mapped)))
    }

    type PeerChunkGetStream =
        Pin<Box<dyn Stream<Item = Result<PeerChunkFrame, Status>> + Send + 'static>>;

    /// ADR 0095: the standing peer-chunk tier's serve arm. Streams
    /// cache-resident, verified-origin chunks by hash; see the proto
    /// comment for the full contract. Deliberately NOT fenced and NOT
    /// wire-version gated at the session level — host-to-host, no
    /// session writes, pure content-addressed reads.
    async fn peer_chunk_get(
        &self,
        req: Request<PeerChunkGetRequest>,
    ) -> Result<Response<Self::PeerChunkGetStream>, Status> {
        use engram_protocol::grpc::peer_chunk_get_request::Scope;
        let Some(peer) = self.peer.clone() else {
            return Err(Status::unavailable(
                "peer-chunk tier disabled: no chunk cache on this host",
            ));
        };
        let req = req.into_inner();
        // Scope: BaseImage is validated against this host's ready set
        // (reject ⇒ the coordinator's hint was stale — requester falls
        // to GCS); Snapshot is counted as-is (no authoritative local
        // snapshot index; hash-capability + resident-only is the gate).
        match &req.scope {
            Some(Scope::BaseImageDigest(digest)) => {
                let digest = engram_protocol::heartbeat::ManifestDigest(digest.clone());
                if !peer.ready.contains(&digest) {
                    metrics::counter!(
                        "engram_peer_serve_total",
                        "outcome" => "scope_reject",
                    )
                    .increment(1);
                    return Err(Status::failed_precondition(format!(
                        "image {} not ready on this host",
                        digest.0
                    )));
                }
            }
            Some(Scope::SnapshotId(_)) | None => {}
        }
        let hashes =
            crate::peer_fill::parse_request_hashes(&req).map_err(Status::invalid_argument)?;
        let Some(permit) = peer.try_claim_stream() else {
            return Err(Status::resource_exhausted(
                "peer serve streams saturated (backpressure — source this batch from GCS)",
            ));
        };
        // Channel depth 4 ≈ 16 MiB in flight per stream at 4 MiB
        // frames — enough to keep the socket busy without buffering
        // whole chunks per item.
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(peer.stream_frames(hashes, tx, permit));
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
    }

    async fn migration_commit(
        &self,
        req: Request<MigrationExportRef>,
    ) -> Result<Response<Empty>, Status> {
        check_wire_version(&req)?;
        let req = req.into_inner();
        let id = decode_sandbox_id(&req.sandbox_id)?;
        let fence = self.check_session_epoch(&req.session_id, req.fencing_epoch)?;
        self.inner
            .migration_commit(id, &req.export_id, fence)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn migration_abort(
        &self,
        req: Request<MigrationExportRef>,
    ) -> Result<Response<Empty>, Status> {
        check_wire_version(&req)?;
        let req = req.into_inner();
        let id = decode_sandbox_id(&req.sandbox_id)?;
        let fence = self.check_session_epoch(&req.session_id, req.fencing_epoch)?;
        self.inner
            .migration_abort(id, &req.export_id, fence)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn migration_abort_unserved(
        &self,
        req: Request<MigrationExportRef>,
    ) -> Result<Response<MigrationAbortUnservedResponse>, Status> {
        check_wire_version(&req)?;
        let req = req.into_inner();
        let id = decode_sandbox_id(&req.sandbox_id)?;
        let fence = self.check_session_epoch(&req.session_id, req.fencing_epoch)?;
        let aborted = self
            .inner
            .migration_abort_unserved(id, &req.export_id, fence)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(MigrationAbortUnservedResponse { aborted }))
    }

    async fn migration_presetup(
        &self,
        req: Request<FencedSandboxRequest>,
    ) -> Result<Response<MigrationPresetupResponse>, Status> {
        let span = tracing::info_span!("host.migration_presetup");
        link_remote_parent(&span, &req);
        check_wire_version(&req)?;
        let r = req.into_inner();
        let id = decode_sandbox_id(&r.uuid)?;
        let fence = self.check_session_epoch(&r.session_id, r.fencing_epoch)?;
        let out = self
            .inner
            .migration_presetup(id, fence)
            .instrument(span)
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
            swap_manifest_ref: out
                .swap_manifest_ref
                .as_ref()
                .map(|value| encode_bincode(value, "ManifestRef"))
                .transpose()?,
            hot_chunks: out.hot_chunks.into_iter().map(|h| h.to_vec()).collect(),
        }))
    }

    async fn migration_capture_post_copy(
        &self,
        req: Request<MigrationExportRef>,
    ) -> Result<Response<PostCopyCaptureResponse>, Status> {
        check_wire_version(&req)?;
        let req = req.into_inner();
        let id = decode_sandbox_id(&req.sandbox_id)?;
        let fence = self.check_session_epoch(&req.session_id, req.fencing_epoch)?;
        let out = self
            .inner
            .migration_capture_postcopy(id, &req.export_id, fence)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(PostCopyCaptureResponse {
            sealed_chunks: out.sealed_chunks,
            total_chunks: out.total_chunks,
            scan_ms: out.scan_ms,
            pause_ms: out.pause_ms,
            disk_drain_ms: out.disk_drain_ms,
            vmstate_ms: out.vmstate_ms,
            sealed_disk_blocks: out.sealed_disk_blocks,
            precopy_disk_blocks: out.precopy_disk_blocks,
            paused_at_unix_ms: out.paused_at_unix_ms,
        }))
    }

    async fn migration_drain_wait(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<DrainOutcomeResponse>, Status> {
        check_wire_version(&req)?;
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
        req: Request<FencedSandboxRequest>,
    ) -> Result<Response<Empty>, Status> {
        check_wire_version(&req)?;
        let r = req.into_inner();
        let fence = self.check_session_epoch(&r.session_id, r.fencing_epoch)?;
        let id = decode_sandbox_id(&r.uuid)?;
        self.inner
            .commit_snapshot(id, fence)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn abort_snapshot(
        &self,
        req: Request<FencedSandboxRequest>,
    ) -> Result<Response<Empty>, Status> {
        check_wire_version(&req)?;
        let r = req.into_inner();
        let fence = self.check_session_epoch(&r.session_id, r.fencing_epoch)?;
        let id = decode_sandbox_id(&r.uuid)?;
        self.inner
            .abort_snapshot(id, fence)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    // ADR 0045 Phase F: freeze / unfreeze a running microVM in place.
    async fn pause_sandbox(
        &self,
        req: Request<FencedSandboxRequest>,
    ) -> Result<Response<Empty>, Status> {
        check_wire_version(&req)?;
        let r = req.into_inner();
        let fence = self.check_session_epoch(&r.session_id, r.fencing_epoch)?;
        let id = decode_sandbox_id(&r.uuid)?;
        self.inner
            .pause(id, fence)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn resume_sandbox(
        &self,
        req: Request<FencedSandboxRequest>,
    ) -> Result<Response<Empty>, Status> {
        check_wire_version(&req)?;
        let r = req.into_inner();
        let fence = self.check_session_epoch(&r.session_id, r.fencing_epoch)?;
        let id = decode_sandbox_id(&r.uuid)?;
        self.inner
            .resume(id, fence)
            .await
            .map_err(sandbox_to_status)?;
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
            let r = req.into_inner();
            let fence = self.check_session_epoch(&r.session_id, r.fencing_epoch)?;
            let metadata = decode_bincode(&r.metadata_bincode, "SnapshotMetadata")?;
            let id = self
                .inner
                .restore(metadata, fence)
                .await
                .map_err(sandbox_to_status)?;
            Ok(Response::new(SandboxIdMessage {
                uuid: id.as_uuid().as_bytes().to_vec(),
            }))
        }
        .instrument(span)
        .await
    }

    /// Drain backend progress before sending its single terminal result.
    async fn materialize_image(
        &self,
        req: Request<MaterializeImageRequest>,
    ) -> Result<Response<Self::MaterializeImageStream>, Status> {
        let span = tracing::info_span!("host.materialize_image");
        link_remote_parent(&span, &req);
        check_wire_version(&req)?;
        let inner = req.into_inner();
        // An empty buffer is an unset proto field → `None` (anonymous /
        // host-ambient auth); a populated buffer is the bincode
        // `Option<ResolvedRegistryAuth>` (a `None` still encodes to a
        // 1-byte discriminant).
        let registry_auth: Option<engram_core::types::registry::ResolvedRegistryAuth> =
            if inner.registry_auth_bincode.is_empty() {
                None
            } else {
                decode_bincode(&inner.registry_auth_bincode, "Option<ResolvedRegistryAuth>")?
            };

        let (progress_tx, mut progress_rx) =
            mpsc::channel::<engram_core::types::MaterializeProgress>(64);
        let (tx, rx) = mpsc::channel::<Result<MaterializeImageEvent, Status>>(16);

        let backend = self.inner.clone();
        let backend_task = tokio::spawn(
            async move {
                backend
                    .materialize_image(
                        &inner.image_uri,
                        &inner.platform_os,
                        &inner.platform_arch,
                        registry_auth,
                        inner.min_disk_gib,
                        progress_tx,
                    )
                    .await
            }
            .instrument(span.clone()),
        );

        tokio::spawn(
            async move {
                while let Some(p) = progress_rx.recv().await {
                    let event = MaterializeImageEvent {
                        event: Some(
                            engram_protocol::grpc::materialize_image_event::Event::Progress(
                                MaterializeProgress {
                                    stage: p.stage.as_str().to_string(),
                                    detail: p.detail,
                                    chunks_done: p.chunks_done,
                                    chunks_total: p.chunks_total,
                                },
                            ),
                        ),
                    };
                    if tx.send(Ok(event)).await.is_err() {
                        // Client dropped the stream. Keep draining
                        // progress_rx (cheap) so the backend task's
                        // sends never block, but stop forwarding.
                        while progress_rx.recv().await.is_some() {}
                        return;
                    }
                }
                let terminal = match backend_task.await {
                    Ok(Ok(out)) => {
                        let done = (|| -> Result<MaterializeImageDone, Status> {
                            Ok(MaterializeImageDone {
                                disk_manifest_bincode: encode_bincode(
                                    &out.disk_manifest,
                                    "ManifestRef",
                                )?,
                                oci_defaults_bincode: encode_bincode(
                                    &out.oci_defaults,
                                    "OciRuntimeDefaults",
                                )?,
                                manifest_digest: out.manifest_digest,
                                ext4_size_bytes: out.ext4_size_bytes,
                            })
                        })();
                        done.map(|d| MaterializeImageEvent {
                            event: Some(
                                engram_protocol::grpc::materialize_image_event::Event::Done(d),
                            ),
                        })
                    }
                    Ok(Err(SandboxError::MaterializeFailed(failure))) => {
                        Ok(MaterializeImageEvent {
                            event: Some(
                                engram_protocol::grpc::materialize_image_event::Event::Failed(
                                    MaterializeImageFailed {
                                        message: failure.message,
                                        kind: failure.kind.as_str().to_string(),
                                    },
                                ),
                            ),
                        })
                    }
                    Ok(Err(other)) => Err(sandbox_to_status(other)),
                    Err(join_err) => Err(Status::internal(format!(
                        "materialize_image backend task panicked: {join_err}"
                    ))),
                };
                let _ = tx.send(terminal).await;
            }
            .instrument(span),
        );

        let out_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Response::new(
            Box::pin(out_stream) as Self::MaterializeImageStream
        ))
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
            let fence = self.check_session_epoch(&inner.session_id, inner.fencing_epoch)?;
            let metadata = decode_bincode(&inner.metadata_bincode, "SnapshotMetadata")?;
            // ADR 0055: empty bytes (old coord / no skills) decode to an empty Vec.
            let selected_mounts = if inner.selected_mounts_bincode.is_empty() {
                Vec::new()
            } else {
                decode_bincode(&inner.selected_mounts_bincode, "selected_mounts")?
            };
            let id = self
                .inner
                .restore_base_for_session(metadata, inner.session_env, selected_mounts, fence)
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
        // The proto's GuestIpResponse.ip is unchanged (optional v4
        // dotted-quad string) — stringify the now-typed Ipv4Addr here,
        // at the wire boundary, rather than change the wire shape.
        let ip = self.inner.guest_ip(id).await.map(|ip| ip.to_string());
        Ok(Response::new(GuestIpResponse { ip }))
    }

    async fn bind_harness_session(
        &self,
        req: Request<BindHarnessSessionRequest>,
    ) -> Result<Response<Empty>, Status> {
        let r = req.into_inner();
        let session_id = decode_session_id(&r.session_id)?;
        let sandbox_id = decode_sandbox_id(&r.sandbox_id)?;
        self.inner
            .bind_session(session_id, sandbox_id, r.binding_epoch)
            .await
            .map_err(sandbox_to_status)?;
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
        let span = tracing::info_span!(
            "host.send_harness_prompt",
            prompt_id = %req.get_ref().prompt_id,
        );
        link_remote_parent(&span, &req);
        async move {
            let r = req.into_inner();
            let sandbox_id = decode_sandbox_id(&r.sandbox_id)?;
            self.inner
                .send_prompt(sandbox_id, r.prompt_id, r.text, r.harness_mode)
                .await
                .map_err(sandbox_to_status)?;
            Ok(Response::new(Empty {}))
        }
        .instrument(span)
        .await
    }

    async fn edit_harness_queued_prompt(
        &self,
        req: Request<EditHarnessQueuedPromptRequest>,
    ) -> Result<Response<Empty>, Status> {
        let r = req.into_inner();
        let sandbox_id = decode_sandbox_id(&r.sandbox_id)?;
        self.inner
            .edit_queued_prompt(sandbox_id, r.prompt_id, r.text)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn dequeue_harness_queued_prompt(
        &self,
        req: Request<DequeueHarnessQueuedPromptRequest>,
    ) -> Result<Response<Empty>, Status> {
        let r = req.into_inner();
        let sandbox_id = decode_sandbox_id(&r.sandbox_id)?;
        self.inner
            .dequeue_queued_prompt(sandbox_id, r.prompt_id)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn send_harness_tool_result(
        &self,
        req: Request<SendHarnessToolResultRequest>,
    ) -> Result<Response<Empty>, Status> {
        let r = req.into_inner();
        let sandbox_id = decode_sandbox_id(&r.sandbox_id)?;
        self.inner
            .tool_result(sandbox_id, r.tool_call_id, r.result_json)
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

    /// Read physical cold-boot inputs through the authenticated host boundary.
    /// This operation cannot allocate a VM, policy lease or network owner.
    async fn read_managed_cold_assets(
        &self,
        req: tonic::Request<engram_protocol::grpc::ReadManagedColdAssetsRequest>,
    ) -> Result<tonic::Response<engram_protocol::grpc::ReadManagedColdAssetsResponse>, tonic::Status>
    {
        self.check_managed_protocol(&req)?;
        let request = engram_protocol::managed_boot::decode(&req.into_inner().payload_json)
            .map_err(sandbox_to_status)?;
        let result = self
            .inner
            .read_managed_cold_assets(request)
            .await
            .map_err(sandbox_to_status)?;
        let payload_json =
            engram_protocol::managed_boot::encode(&result).map_err(sandbox_to_status)?;
        Ok(Response::new(
            engram_protocol::grpc::ReadManagedColdAssetsResponse { payload_json },
        ))
    }
    async fn capture_managed_boot(
        &self,
        req: tonic::Request<engram_protocol::grpc::ManagedBootRequest>,
    ) -> Result<tonic::Response<engram_protocol::grpc::ManagedBootResponse>, tonic::Status> {
        self.check_managed_protocol(&req)?;
        let request = engram_protocol::managed_boot::decode(&req.into_inner().payload_json)
            .map_err(sandbox_to_status)?;
        let result = self
            .inner
            .capture_managed_boot(request)
            .await
            .map_err(sandbox_to_status)?;
        let payload_json =
            engram_protocol::managed_boot::encode(&result).map_err(sandbox_to_status)?;
        Ok(Response::new(engram_protocol::grpc::ManagedBootResponse {
            payload_json,
        }))
    }
    async fn prepare_managed_boot(
        &self,
        req: tonic::Request<engram_protocol::grpc::ManagedBootRequest>,
    ) -> Result<tonic::Response<engram_protocol::grpc::ManagedBootResponse>, tonic::Status> {
        self.check_managed_protocol(&req)?;
        let request = engram_protocol::managed_boot::decode(&req.into_inner().payload_json)
            .map_err(sandbox_to_status)?;
        let result = self
            .inner
            .prepare_managed_boot(request)
            .await
            .map_err(sandbox_to_status)?;
        let payload_json =
            engram_protocol::managed_boot::encode(&result).map_err(sandbox_to_status)?;
        Ok(Response::new(engram_protocol::grpc::ManagedBootResponse {
            payload_json,
        }))
    }
    async fn read_managed_boot(
        &self,
        req: tonic::Request<engram_protocol::grpc::ManagedBootRequest>,
    ) -> Result<tonic::Response<engram_protocol::grpc::ManagedBootResponse>, tonic::Status> {
        self.check_managed_protocol(&req)?;
        let request = engram_protocol::managed_boot::decode(&req.into_inner().payload_json)
            .map_err(sandbox_to_status)?;
        let result = self
            .inner
            .read_managed_boot(request)
            .await
            .map_err(sandbox_to_status)?;
        let payload_json =
            engram_protocol::managed_boot::encode(&result).map_err(sandbox_to_status)?;
        Ok(Response::new(engram_protocol::grpc::ManagedBootResponse {
            payload_json,
        }))
    }
    async fn finalize_managed_boot(
        &self,
        req: tonic::Request<engram_protocol::grpc::ManagedBootRequest>,
    ) -> Result<tonic::Response<engram_protocol::grpc::ManagedBootResponse>, tonic::Status> {
        self.check_managed_protocol(&req)?;
        let request = engram_protocol::managed_boot::decode(&req.into_inner().payload_json)
            .map_err(sandbox_to_status)?;
        let result = self
            .inner
            .finalize_managed_boot(request)
            .await
            .map_err(sandbox_to_status)?;
        let payload_json =
            engram_protocol::managed_boot::encode(&result).map_err(sandbox_to_status)?;
        Ok(Response::new(engram_protocol::grpc::ManagedBootResponse {
            payload_json,
        }))
    }
    async fn start_managed_boot(
        &self,
        req: tonic::Request<engram_protocol::grpc::ManagedBootRequest>,
    ) -> Result<tonic::Response<engram_protocol::grpc::ManagedBootResponse>, tonic::Status> {
        self.check_managed_protocol(&req)?;
        let request = engram_protocol::managed_boot::decode(&req.into_inner().payload_json)
            .map_err(sandbox_to_status)?;
        let result = self
            .inner
            .start_managed_boot(request)
            .await
            .map_err(sandbox_to_status)?;
        let payload_json =
            engram_protocol::managed_boot::encode(&result).map_err(sandbox_to_status)?;
        Ok(Response::new(engram_protocol::grpc::ManagedBootResponse {
            payload_json,
        }))
    }
    async fn read_managed_policy(
        &self,
        req: Request<engram_protocol::grpc::ReadManagedPolicyRequest>,
    ) -> Result<Response<engram_protocol::grpc::ReadManagedPolicyResponse>, Status> {
        self.check_managed_protocol(&req)?;
        if req.get_ref().target_bincode.len() > 4096 {
            return Err(Status::resource_exhausted("managed request too large"));
        }
        let target = engram_protocol::managed_policy::decode(&req.into_inner().target_bincode)
            .map_err(|_| Status::invalid_argument("invalid managed target"))?;
        let state = self
            .inner
            .read_managed_policy(target)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(
            engram_protocol::grpc::ReadManagedPolicyResponse {
                state_bincode: bincode::serialize(&state)
                    .map_err(|_| Status::internal("managed response encoding failed"))?,
            },
        ))
    }
    async fn apply_managed_policy(
        &self,
        req: Request<engram_protocol::grpc::ApplyManagedPolicyRequest>,
    ) -> Result<Response<engram_protocol::grpc::ApplyManagedPolicyResponse>, Status> {
        self.check_managed_protocol(&req)?;
        if req.get_ref().request_bincode.len() > 4 * 1024 * 1024 {
            return Err(Status::resource_exhausted("managed request too large"));
        }
        let request = engram_protocol::managed_policy::decode(&req.into_inner().request_bincode)
            .map_err(|_| Status::invalid_argument("invalid managed request"))?;
        let state = self
            .inner
            .apply_managed_policy(request)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(
            engram_protocol::grpc::ApplyManagedPolicyResponse {
                state_bincode: bincode::serialize(&state)
                    .map_err(|_| Status::internal("managed response encoding failed"))?,
            },
        ))
    }
    async fn start_agent(
        &self,
        req: Request<StartAgentRequest>,
    ) -> Result<Response<Empty>, Status> {
        // `agent_handshake` is the suspected dominant cold-boot phase
        // (in-VM kernel boot + ext4 mount + agentd dial);
        // this span makes it visible in the end-to-end trace (ADR 0019).
        let span = tracing::info_span!("host.start_agent", phase = "agent_handshake");
        link_remote_parent(&span, &req);
        check_wire_version(&req)?;
        async move {
            let r = req.into_inner();
            let sandbox_id = decode_sandbox_id(&r.sandbox_id)?;
            let agent = decode_bincode(&r.agent_bincode, "AgentSpec")?;
            let policy = engram_protocol::ordinary_policy::decode(&r.policy_bincode)
                .map_err(sandbox_to_status)?;
            let fence = self.check_session_epoch(&r.session_id, r.fencing_epoch)?;
            let phase_start = crate::time_source::metrics_now();
            let result = self
                .inner
                .start_agent(sandbox_id, agent, policy, fence)
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

    async fn start_browser(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<BrowserPortResponse>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        let start = self
            .inner
            .start_browser(id)
            .await
            .map_err(sandbox_to_status)?;
        // Issue #569: agentd's chromium-CDP liveness warning — the browser
        // stack's x11vnc is up (the RPC succeeded) but chrome may be dead or
        // crash-looping behind it. Log with the sandbox id and forward to the
        // coordinator; diagnostic only, never a failure.
        if let Some(warning) = &start.warning {
            tracing::warn!(sandbox_id = %id, %warning, "start_browser: chromium liveness warning");
        }
        Ok(Response::new(BrowserPortResponse {
            port: u32::from(start.port),
            warning: start.warning,
        }))
    }

    async fn stop_browser(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<Empty>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        self.inner
            .stop_browser(id)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn start_ide(
        &self,
        req: Request<SandboxIdMessage>,
    ) -> Result<Response<IdePortResponse>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        let port = self.inner.start_ide(id).await.map_err(sandbox_to_status)?;
        Ok(Response::new(IdePortResponse {
            port: u32::from(port),
        }))
    }

    async fn stop_ide(&self, req: Request<SandboxIdMessage>) -> Result<Response<Empty>, Status> {
        let id = decode_sandbox_id(&req.into_inner().uuid)?;
        self.inner.stop_ide(id).await.map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    /// Server-streaming exec. First frame is `started` (carries the
    /// canonical `exec_id`); subsequent frames carry stdout /
    /// stderr bytes. A backend-produced `Exit` is forwarded verbatim;
    /// backend closure before `Exit` terminates the RPC with `Unavailable`.
    async fn exec_start(
        &self,
        req: Request<ExecStartRequest>,
    ) -> Result<Response<Self::ExecStartStream>, Status> {
        check_wire_version(&req)?;
        let r = req.into_inner();
        let sandbox_id = decode_sandbox_id(&r.sandbox_id)?;
        let wire: WireExecRequest = decode_bincode(&r.request_bincode, "WireExecRequest")?;
        let mut request = wire.into_engine();
        request.exec_id = r.exec_id;
        request.stdout_offset = r.stdout_offset;
        request.stderr_offset = r.stderr_offset;
        request.wake = r.wake;

        let stream = self
            .inner
            .exec_stream(sandbox_id, request)
            .await
            .map_err(sandbox_to_status)?;

        // mpsc channel feeds the gRPC stream out. A backend-produced Exit or
        // Refused is terminal; closure first becomes Unavailable.
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

        tokio::spawn(pump_exec_frames(stream.exec_id, stream.events, tx));

        let out_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(out_stream) as Self::ExecStartStream))
    }

    async fn cancel_exec(
        &self,
        req: Request<CancelExecRequest>,
    ) -> Result<Response<Empty>, Status> {
        check_wire_version(&req)?;
        let request = req.into_inner();
        let sandbox_id = decode_sandbox_id(&request.sandbox_id)?;
        self.inner
            .cancel_exec(sandbox_id, request.exec_id)
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(Empty {}))
    }

    async fn write_file(
        &self,
        req: Request<tonic::Streaming<HostWriteFileRequest>>,
    ) -> Result<Response<HostWriteFileResponse>, Status> {
        check_wire_version(&req)?;
        use futures::StreamExt;
        let mut inbound = req.into_inner();
        let first = inbound
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("file metadata frame is required"))??;
        let metadata = match first.frame {
            Some(engram_protocol::grpc::host_write_file_request::Frame::Metadata(metadata)) => {
                metadata
            }
            _ => {
                return Err(Status::invalid_argument(
                    "first file frame must be metadata",
                ));
            }
        };
        let sandbox_id = decode_sandbox_id(&metadata.sandbox_id)?;
        let spec = SessionFileSpec {
            path: metadata.path,
            size_bytes: metadata.size_bytes,
            sha256: metadata.sha256,
            mode: metadata.mode,
        };
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            while let Some(frame) = inbound.next().await {
                let item = match frame {
                    Ok(HostWriteFileRequest {
                        frame:
                            Some(engram_protocol::grpc::host_write_file_request::Frame::Chunk(chunk)),
                    }) => Ok(bytes::Bytes::from(chunk)),
                    Ok(_) => Err(SandboxError::InvalidSpec(
                        "file metadata must appear exactly once".into(),
                    )),
                    Err(error) => Err(SandboxError::Unavailable(format!(
                        "file stream transport failed: {error}"
                    ))),
                };
                if tx.send(item).await.is_err() {
                    return;
                }
            }
        });
        let result = self
            .inner
            .write_file(
                sandbox_id,
                spec,
                Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
            )
            .await
            .map_err(sandbox_to_status)?;
        Ok(Response::new(HostWriteFileResponse {
            path: result.path,
            size_bytes: result.size_bytes,
            sha256: result.sha256,
        }))
    }

    async fn read_file(
        &self,
        req: Request<HostReadFileRequest>,
    ) -> Result<Response<Self::ReadFileStream>, Status> {
        check_wire_version(&req)?;
        use futures::StreamExt;
        let request = req.into_inner();
        let sandbox_id = decode_sandbox_id(&request.sandbox_id)?;
        let (metadata, bytes) = self
            .inner
            .read_file(sandbox_id, request.path)
            .await
            .map_err(sandbox_to_status)?;
        let metadata = HostReadFileResponse {
            frame: Some(
                engram_protocol::grpc::host_read_file_response::Frame::Metadata(
                    HostReadFileMetadata {
                        path: metadata.path,
                        size_bytes: metadata.size_bytes,
                        sha256: metadata.sha256,
                    },
                ),
            ),
        };
        // The response owns the source, so downstream cancellation drops it even
        // while its next chunk is pending.
        let response =
            futures::stream::once(async move { Ok(metadata) }).chain(bytes.map(|item| {
                item.map(|chunk| HostReadFileResponse {
                    frame: Some(
                        engram_protocol::grpc::host_read_file_response::Frame::Chunk(
                            chunk.to_vec(),
                        ),
                    ),
                })
                .map_err(sandbox_to_status)
            }));
        Ok(Response::new(Box::pin(response)))
    }

    /// ADR 0014 issue #6: bidi WS-frame tunnel.
    ///
    /// Wire contract (ADR 0015 follow-up): the FIRST message MUST be a `ProxyShellOpen`
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
        //
        // WEAK for the same reason as `proxy_port`: a strong clone would
        // hold the response stream open after ttyd ends until the client
        // closes.
        let out_tx_for_open_reject = out_tx.downgrade();
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
                        if let Some(tx) = out_tx_for_open_reject.upgrade() {
                            let _ = tx
                                .send(Err(Status::invalid_argument(format!("proxy_shell: {e}"))))
                                .await;
                        }
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

    /// ADR 0064: bidi RAW-BYTE port tunnel — `proxy_shell`'s sibling.
    /// Same handshake discipline (the FIRST message MUST be
    /// `ProxyPortOpen{sandbox_id, port}`; any other first variant, or a
    /// later `Open`, is `InvalidArgument`), but the payload is opaque
    /// `Data` byte chunks plus a `Close` sentinel — no WS frame
    /// taxonomy, because this is a plain TCP pipe to a dev server.
    async fn proxy_port(
        &self,
        req: Request<tonic::Streaming<ProxyPortMessage>>,
    ) -> Result<Response<Self::ProxyPortStream>, Status> {
        let mut inbound = req.into_inner();

        use futures::StreamExt;
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), inbound.next())
            .await
            .map_err(|_| Status::deadline_exceeded("proxy_port: no first message within 5s"))?
            .ok_or_else(|| Status::cancelled("proxy_port: client closed before first message"))?
            .map_err(|e| Status::internal(format!("proxy_port: recv first message: {e}")))?;
        let (sandbox_id, port) = match first.body {
            Some(ProxyPortBody::Open(open)) => {
                let sid = decode_sandbox_id(&open.sandbox_id)?;
                let port = u16::try_from(open.port)
                    .ok()
                    .filter(|p| *p != 0)
                    .ok_or_else(|| {
                        Status::invalid_argument(format!(
                            "proxy_port: port {} out of range (1..=65535)",
                            open.port
                        ))
                    })?;
                (sid, port)
            }
            Some(_) => {
                return Err(Status::invalid_argument(
                    "proxy_port: first message must be Open",
                ));
            }
            None => {
                return Err(Status::invalid_argument(
                    "proxy_port: first message has empty body",
                ));
            }
        };

        let tunnel = self
            .inner
            .proxy_port(sandbox_id, port)
            .await
            .map_err(sandbox_to_status)?;
        let engram_core::types::port::PortTunnel {
            outbound: tunnel_outbound,
            inbound: mut tunnel_inbound,
        } = tunnel;

        // mpsc carrying byte chunks out to the gRPC client.
        let (out_tx, out_rx) = mpsc::channel::<Result<ProxyPortMessage, Status>>(64);

        // gRPC inbound (client → us): forward Data into the tunnel
        // outbound; Close / client-disconnect tears the tunnel down. An
        // Open after the handshake is a protocol error (typed status, so
        // the client surfaces a clean error rather than an opaque close).
        //
        // WEAK on purpose: the response stream ends only when every sender
        // is dropped. A strong clone here would hold it open until the
        // CLIENT closes, so a guest EOF (the end of a close-delimited HTTP
        // response) never reached the client and the browser waited for the
        // body forever.
        let out_tx_for_reject = out_tx.downgrade();
        tokio::spawn(async move {
            while let Some(next) = inbound.next().await {
                let msg = match next {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(error = %e, "proxy_port: client recv error");
                        break;
                    }
                };
                match msg.body {
                    Some(ProxyPortBody::Data(d)) => {
                        if tunnel_outbound.send(d.data.into()).await.is_err() {
                            break;
                        }
                    }
                    Some(ProxyPortBody::Close(_)) | None => break,
                    Some(ProxyPortBody::Open(_)) => {
                        if let Some(tx) = out_tx_for_reject.upgrade() {
                            let _ = tx
                                .send(Err(Status::invalid_argument(
                                    "proxy_port: Open only valid as the first message",
                                )))
                                .await;
                        }
                        break;
                    }
                }
            }
            // Client closed; drop the tunnel outbound so the host pump
            // half-closes the guest socket and tears down.
            drop(tunnel_outbound);
        });

        // Tunnel inbound (guest → us): forward each byte chunk to the
        // gRPC client as a Data message.
        tokio::spawn(async move {
            while let Some(chunk) = tunnel_inbound.recv().await {
                let msg = ProxyPortMessage {
                    body: Some(ProxyPortBody::Data(ProxyPortData {
                        data: chunk.to_vec(),
                    })),
                };
                if out_tx.send(Ok(msg)).await.is_err() {
                    break;
                }
            }
        });

        let out_stream = tokio_stream::wrappers::ReceiverStream::new(out_rx);
        Ok(Response::new(Box::pin(out_stream) as Self::ProxyPortStream))
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

/// Reject an explicit incompatible version before decoding a payload.
/// An absent header retains the ordinary HTTP/2 predecessor contract.
fn check_wire_version<T>(req: &Request<T>) -> Result<(), Status> {
    let Some(raw) = req
        .metadata()
        .get(engram_protocol::wire::WIRE_VERSION_METADATA_KEY)
    else {
        return Ok(());
    };
    let Some(coord) = raw.to_str().ok().and_then(|s| s.parse::<u32>().ok()) else {
        return Err(Status::failed_precondition("invalid host protocol version"));
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
        // ADR 0068: the host-agent never ORIGINATES `Unsupported` from
        // its local backend (`SandboxBackend::probe_sandbox`'s default
        // impl always answers; `Unimplemented` is what the CLIENT sees
        // against an old host-agent that doesn't have this handler at
        // all, not something this handler itself would ever return).
        // Map defensively in case a future refactor surfaces it here.
        SandboxError::Unsupported(_) => Status::unimplemented(err.to_string()),
        // ADR 0084 P1b: the `BuildBaseSnapshot` RPC that used to
        // pattern-match this variant and emit a structured `CaptureFailed`
        // stream frame (so the kind/stage/tail survived the wire) is
        // deleted — capture failures now travel as a `CaptureTerminalReport`
        // on the heartbeat, never through this gRPC error-mapping path.
        // Kept as a defensive fallback in case any other caller ever
        // surfaces this variant through a non-streaming RPC.
        SandboxError::CaptureFailed(failure) => Status::internal(failure.to_string()),
        // ADR 0080: the streaming `materialize_image` handler emits a
        // structured `MaterializeImageFailed` frame for this variant
        // (so the kind survives the wire); same defensive fallback as
        // CaptureFailed for any future non-streaming caller.
        SandboxError::MaterializeFailed(failure) => Status::internal(failure.to_string()),
        // ADR 0116 B-D4: the guest's io::ErrorKind rides a marker
        // message on `failed_precondition`, exactly the wire-skew
        // shape — the coord-side client parses it back to the typed
        // variant; an older coordinator sees an ordinary status and
        // behaves as before (no WIRE_VERSION bump).
        SandboxError::HarnessSpawn { kind, message } => Status::failed_precondition(
            engram_protocol::wire::harness_spawn_message(&kind, &message),
        ),
        // Same marker shape: the coordinator reads the typed variant and
        // recovers the session from its disk instead of retrying.
        SandboxError::ManagedRecoveryRequired => {
            Status::failed_precondition(engram_protocol::wire::MANAGED_RECOVERY_REQUIRED_STATUS)
        }
        SandboxError::MemoryImageUnusable(message) => Status::failed_precondition(
            engram_protocol::wire::memory_image_unusable_message(&message),
        ),
    }
}

#[cfg(test)]
mod wire_version_tests {
    use super::*;
    use bytes::Bytes;
    use engram_core::types::sandbox::ExecEventStream;
    use engram_protocol::wire::WIRE_VERSION_METADATA_KEY;
    use futures::StreamExt;

    #[test]
    fn epoch_zero_preserves_session_identity_without_high_water_access() {
        let work = tempfile::tempdir().unwrap();
        let backend = Arc::new(engram_sandbox_process::ProcessBackend::new(work.path()));
        let host = Arc::new(crate::host_client::LocalHostClient::with_noop_hub(backend));
        let path = work.path().join("epochs");
        let epochs = SessionEpochStore::open(&path).unwrap();
        let service = HostServiceImpl::new(host, epochs.clone());
        let session = engram_core::SessionId::new();
        let uuid = session.as_uuid();
        let bytes = uuid.as_bytes();

        assert_eq!(
            service.check_session_epoch(&[], 0).unwrap(),
            SessionFence::unfenced()
        );
        assert_eq!(
            service.check_session_epoch(&[0; 16], 0).unwrap(),
            SessionFence::unfenced()
        );
        assert_eq!(
            service.check_session_epoch(bytes, 0).unwrap(),
            SessionFence::new(session, 0)
        );
        assert!(epochs.read(session).unwrap().is_none());
        assert_eq!(
            service.check_session_epoch(bytes, 5).unwrap(),
            SessionFence::new(session, 5)
        );
        assert_eq!(
            service.check_session_epoch(bytes, 0).unwrap(),
            SessionFence::new(session, 0)
        );
        assert_eq!(epochs.read(session).unwrap().unwrap().epoch, 5);
        let stale = service.check_session_epoch(bytes, 4).unwrap_err();
        assert_eq!(stale.code(), tonic::Code::FailedPrecondition);
        assert!(stale.message().contains("stored high-water 5, incoming 4"));
        assert_eq!(epochs.read(session).unwrap().unwrap().epoch, 5);

        // A zero epoch must not read or repair even a corrupt high-water record.
        let record = path.join(format!("{session}.json"));
        std::fs::write(&record, b"corrupt").unwrap();
        assert_eq!(
            service.check_session_epoch(bytes, 0).unwrap(),
            SessionFence::new(session, 0)
        );
        assert_eq!(std::fs::read(record).unwrap(), b"corrupt");
    }

    #[test]
    fn epoch_zero_rejects_malformed_session_identity() {
        let work = tempfile::tempdir().unwrap();
        let backend = Arc::new(engram_sandbox_process::ProcessBackend::new(work.path()));
        let host = Arc::new(crate::host_client::LocalHostClient::with_noop_hub(backend));
        let service = HostServiceImpl::new(host, crate::session_epochs::ephemeral());
        for bytes in [&[1][..], &[1; 15][..], &[1; 17][..]] {
            assert_eq!(
                service.check_session_epoch(bytes, 0).unwrap_err().code(),
                tonic::Code::InvalidArgument
            );
        }
    }

    fn exec_events<I>(events: I) -> ExecEventStream
    where
        I: IntoIterator<Item = ExecEvent>,
        I::IntoIter: Send + 'static,
    {
        Box::pin(futures::stream::iter(events))
    }

    async fn pump_items(events: ExecEventStream) -> Vec<Result<ExecFrame, Status>> {
        let (tx, rx) = mpsc::channel(16);
        pump_exec_frames("exec:pump-test".into(), events, tx).await;
        tokio_stream::wrappers::ReceiverStream::new(rx)
            .collect()
            .await
    }

    #[tokio::test]
    async fn exec_pump_closure_without_exit_sends_unavailable_not_exit() {
        let items = pump_items(exec_events([ExecEvent::Stdout(Bytes::from_static(
            b"partial",
        ))]))
        .await;

        assert_eq!(items.len(), 2);
        assert!(matches!(
            &items[0],
            Ok(ExecFrame {
                frame: Some(engram_protocol::grpc::exec_frame::Frame::Stdout(bytes)),
            }) if bytes == b"partial"
        ));
        match &items[1] {
            Err(status) => {
                assert_eq!(status.code(), tonic::Code::Unavailable);
                assert!(status.message().contains("exec:pump-test"));
                assert!(status.message().contains("recoverable"));
                assert!(status.message().contains("re-attaching"));
            }
            Ok(frame) => panic!("expected Unavailable, not frame {frame:?}"),
        }
        assert!(!items.iter().any(|item| {
            matches!(
                item,
                Ok(ExecFrame {
                    frame: Some(engram_protocol::grpc::exec_frame::Frame::Exit(_)),
                })
            )
        }));
    }

    #[tokio::test]
    async fn exec_pump_forwards_output_and_real_exit_verbatim() {
        let items = pump_items(exec_events([
            ExecEvent::Stdout(Bytes::from_static(b"out")),
            ExecEvent::Stderr(Bytes::from_static(b"err")),
            ExecEvent::Exit(Some(7)),
            ExecEvent::Stdout(Bytes::from_static(b"after-exit")),
        ]))
        .await;

        assert_eq!(items.len(), 3);
        assert!(matches!(
            &items[0],
            Ok(ExecFrame {
                frame: Some(engram_protocol::grpc::exec_frame::Frame::Stdout(bytes)),
            }) if bytes == b"out"
        ));
        assert!(matches!(
            &items[1],
            Ok(ExecFrame {
                frame: Some(engram_protocol::grpc::exec_frame::Frame::Stderr(bytes)),
            }) if bytes == b"err"
        ));
        assert!(matches!(
            &items[2],
            Ok(ExecFrame {
                frame: Some(engram_protocol::grpc::exec_frame::Frame::Exit(ExecExit {
                    status: Some(7),
                })),
            })
        ));
    }

    #[tokio::test]
    async fn exec_pump_preserves_backend_exit_none() {
        let items = pump_items(exec_events([ExecEvent::Exit(None)])).await;

        assert_eq!(items.len(), 1);
        assert!(matches!(
            &items[0],
            Ok(ExecFrame {
                frame: Some(engram_protocol::grpc::exec_frame::Frame::Exit(ExecExit {
                    status: None,
                })),
            })
        ));
    }

    #[tokio::test]
    async fn exec_pump_forwards_refused_as_distinct_terminal_frame() {
        let items = pump_items(exec_events([
            ExecEvent::Refused("first writer wins".into()),
            ExecEvent::Stdout(Bytes::from_static(b"after-refusal")),
        ]))
        .await;

        assert_eq!(items.len(), 1);
        assert!(matches!(
            &items[0],
            Ok(ExecFrame {
                frame: Some(engram_protocol::grpc::exec_frame::Frame::Refused(reason)),
            }) if reason == "first writer wins"
        ));
    }

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

    #[tokio::test]
    async fn cold_assets_route_requires_authenticated_mode_and_wire_match() {
        let work = tempfile::tempdir().unwrap();
        let backend = Arc::new(engram_sandbox_process::ProcessBackend::new(work.path()));
        let host = Arc::new(crate::host_client::LocalHostClient::with_noop_hub(backend));
        let mut service = HostServiceImpl::new(host, crate::session_epochs::ephemeral());
        let req = || {
            let mut request = Request::new(engram_protocol::grpc::ReadManagedColdAssetsRequest {
                payload_json: b"null".to_vec(),
            });
            request.metadata_mut().insert(
                WIRE_VERSION_METADATA_KEY,
                engram_protocol::WIRE_VERSION.to_string().parse().unwrap(),
            );
            request.metadata_mut().insert(
                engram_protocol::wire::MANAGED_PROTOCOL_METADATA_KEY,
                engram_protocol::wire::MANAGED_PROTOCOL_VERSION
                    .to_string()
                    .parse()
                    .unwrap(),
            );
            request
        };
        assert_eq!(
            service
                .read_managed_cold_assets(req())
                .await
                .unwrap_err()
                .code(),
            tonic::Code::FailedPrecondition
        );
        service.managed_tls = true;
        let mut skew = req();
        skew.metadata_mut().insert(
            WIRE_VERSION_METADATA_KEY,
            (engram_protocol::WIRE_VERSION + 1)
                .to_string()
                .parse()
                .unwrap(),
        );
        assert_eq!(
            service
                .read_managed_cold_assets(skew)
                .await
                .unwrap_err()
                .code(),
            tonic::Code::FailedPrecondition
        );
        assert_eq!(
            service
                .read_managed_cold_assets(req())
                .await
                .unwrap_err()
                .code(),
            tonic::Code::InvalidArgument
        );
    }

    #[tokio::test]
    async fn migration_fetch_rejects_wire_skew_before_artifacts() {
        let work = tempfile::tempdir().unwrap();
        let backend = Arc::new(engram_sandbox_process::ProcessBackend::new(work.path()));
        let host = Arc::new(crate::host_client::LocalHostClient::with_noop_hub(backend));
        let service = HostServiceImpl::new(host, crate::session_epochs::ephemeral());
        let mut req = Request::new(MigrationFetchRequest {
            export_id: "absent-export".into(),
            items: vec![engram_protocol::grpc::MigrationItem {
                role: 0,
                kind: engram_protocol::grpc::migration_item::Kind::StateBin as i32,
                index: 0,
            }],
        });
        req.metadata_mut().insert(
            WIRE_VERSION_METADATA_KEY,
            (engram_protocol::WIRE_VERSION + 1)
                .to_string()
                .parse()
                .unwrap(),
        );
        let Err(status) = service.migration_fetch(req).await else {
            panic!("wire skew must fail before serving artifacts");
        };
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(engram_protocol::wire::parse_wire_skew_message(status.message()).is_some());
    }

    #[tokio::test]
    async fn migration_fetch_rejects_retired_and_unspecified_kinds() {
        let work = tempfile::tempdir().unwrap();
        let backend = Arc::new(engram_sandbox_process::ProcessBackend::new(work.path()));
        let host = Arc::new(crate::host_client::LocalHostClient::with_noop_hub(backend));
        let service = HostServiceImpl::new(host, crate::session_epochs::ephemeral());
        for kind in [0, 2, 3, 5] {
            let mut req = Request::new(MigrationFetchRequest {
                export_id: "absent-export".into(),
                items: vec![engram_protocol::grpc::MigrationItem {
                    role: 0,
                    kind,
                    index: 0,
                }],
            });
            req.metadata_mut().insert(
                WIRE_VERSION_METADATA_KEY,
                engram_protocol::WIRE_VERSION.to_string().parse().unwrap(),
            );
            let Err(status) = service.migration_fetch(req).await else {
                panic!("retired or unspecified kind must fail");
            };
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
        }
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
    fn unparseable_version_header_is_rejected() {
        // A declared version must be parseable before payload decoding.
        let mut req = Request::new(());
        req.metadata_mut()
            .insert(WIRE_VERSION_METADATA_KEY, "not-a-number".parse().unwrap());
        assert!(check_wire_version(&req).is_err());
    }

    #[test]
    fn harness_spawn_error_maps_to_failed_precondition_marker() {
        // ADR 0116 B-D4: a spawn rejection must survive `sandbox_to_status`
        // as a parseable marker (the wire-skew shape), never fold into
        // `Status::internal` with the Snapshot/Io/Vm bucket.
        let status = sandbox_to_status(SandboxError::HarnessSpawn {
            kind: "NotFound".into(),
            message: "spawn_harness: spawn \"/opt/engram/dyn/0/bin/h\": ENOENT".into(),
        });
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            engram_protocol::wire::parse_harness_spawn_message(status.message()),
            Some((
                "NotFound".to_string(),
                "spawn_harness: spawn \"/opt/engram/dyn/0/bin/h\": ENOENT".to_string()
            )),
        );
    }

    #[test]
    fn managed_recovery_maps_to_exact_failed_precondition_marker() {
        let status = sandbox_to_status(SandboxError::ManagedRecoveryRequired);
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            status.message(),
            engram_protocol::wire::MANAGED_RECOVERY_REQUIRED_STATUS
        );
    }

    #[test]
    fn memory_image_unusable_maps_to_failed_precondition_marker() {
        let status = sandbox_to_status(SandboxError::MemoryImageUnusable(
            "swap restore has no manifest".into(),
        ));
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            engram_protocol::wire::parse_memory_image_unusable_message(status.message()),
            Some("swap restore has no manifest".to_string()),
        );
    }
}
