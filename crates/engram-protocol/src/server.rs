//! Host-side WebSocket server: dispatches incoming frames to a local
//! [`HostClient`] and streams responses back to the coordinator.
//!
//! The host is the *server* of the HostClient RPC even though it
//! dialled the coordinator — once the WebSocket is up, frames flow
//! both ways. The host originates only Notifies (Hello, Heartbeat);
//! all Requests come from the coordinator.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use engram_core::traits::HostClient;
use engram_core::types::sandbox::ExecEvent;
use engram_core::SandboxError;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use tokio::sync::{oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;

use crate::codec;
use crate::heartbeat::{Heartbeat, HeartbeatAck};
use crate::wire::{
    Frame, NotifyKind, RemoteError, RequestKind, ResponseKind, StreamItem, TraceContext,
    WireReapStats,
};

/// Hooks the coordinator side of the server can install to react to
/// inbound notifications.
#[async_trait]
pub trait NotifyHandler: Send + Sync {
    /// Called for every `NotifyKind::Hello` (typically once per
    /// connection lifetime). The coordinator binds the host_id here.
    ///
    /// `wire_version` is the host-agent's [`crate::WIRE_VERSION`]
    /// constant. Implementations should compare it to their own
    /// `WIRE_VERSION` and log loudly (or refuse subsequent frames)
    /// on mismatch — bincode is schemaless positional encoding, so
    /// a mismatch silently misaligns every byte that follows.
    async fn on_hello(
        &self,
        host_id: engram_core::HostId,
        agent_version: String,
        wire_version: u32,
    );

    /// Called for each `NotifyKind::Heartbeat`. The coordinator
    /// updates `hosts.last_heartbeat_at` and refreshes its in-memory
    /// view of capacity / local-snapshot state.
    async fn on_heartbeat(&self, hb: Heartbeat) -> HeartbeatAck;
}

/// Host-side admin operations the coord can fan out via WS-RPC.
///
/// Distinct from [`HostClient`] because these aren't session-
/// lifecycle calls — they're per-host operational primitives the
/// coord drives on a cadence or via `POST /api/admin/*`.
///
/// `None` on `serve()` is the supported zero-config default — the
/// host returns `RemoteError::Other("...not configured")` for any
/// admin RPC, which the coord surfaces as an aggregate failure in
/// the fanout response.
#[async_trait]
pub trait HostAdminHandler: Send + Sync {
    /// Sweep the host's local materialize_dir. `live_disk_manifest_ids`
    /// is the coord's authoritative live set; anything not in it +
    /// older than `min_age_secs` gets reaped.
    async fn reap_materialize_dir(
        &self,
        min_age_secs: u64,
        live_disk_manifest_ids: Vec<uuid::Uuid>,
    ) -> Result<WireReapStats, String>;
}

/// Server side: read frames from a WS connection and dispatch them.
///
/// `backend` services Request frames; `notify_handler` is called for
/// inbound Notifies. `admin_handler` (optional) handles host-admin
/// RPCs like `ReapMaterializeDir`; `None` rejects those with a
/// typed error. Returns when the read half ends or errors.
pub async fn serve<W, R>(
    backend: Arc<dyn HostClient>,
    notify_handler: Option<Arc<dyn NotifyHandler>>,
    admin_handler: Option<Arc<dyn HostAdminHandler>>,
    writer: W,
    reader: R,
) where
    W: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + Send
        + Unpin
        + 'static,
    R: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Send
        + Unpin
        + 'static,
{
    let session = HostSession::new(writer);
    session
        .serve_with_reader(backend, notify_handler, admin_handler, reader)
        .await
}

/// Host-side connection handle. Owns the WS writer behind a mutex so
/// the dialer can send outbound Notifies (Hello, Heartbeat) while the
/// reader loop concurrently handles inbound Requests.
///
/// As of ADR 0007 the host also issues outbound Requests
/// (`ResolveRegistryAuth` for OCI auth resolution against the coord's
/// `PgAuthResolver`). The reverse-direction RPC uses the same wire
/// shape: outbound `Frame::Request { req_id }`, inbound
/// `Frame::Response { req_id }`. The reader loop demuxes responses
/// against `pending`, mirroring `ConnectedHost::demux_loop` on the
/// coord side.
#[derive(Clone)]
pub struct HostSession {
    writer: SharedSink,
    /// Outbound-request bookkeeping. `req_id → oneshot for the response`.
    pending: Arc<DashMap<u64, oneshot::Sender<Result<ResponseKind, RemoteError>>>>,
    next_id: Arc<AtomicU64>,
}

impl HostSession {
    pub fn new<W>(writer: W) -> Self
    where
        W: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
            + Send
            + Unpin
            + 'static,
    {
        Self {
            writer: Arc::new(Mutex::new(Box::new(writer))),
            pending: Arc::new(DashMap::new()),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Send an outbound Notify (e.g. Hello at connect, periodic Heartbeat).
    pub async fn notify(&self, notify: NotifyKind) -> Result<(), String> {
        send_frame_typed(&self.writer, Frame::Notify(notify)).await
    }

    /// Issue an outbound Request and await its Response. Used by
    /// host-initiated RPCs against the coord (e.g.
    /// `ResolveRegistryAuth`).
    pub async fn request(&self, kind: RequestKind) -> Result<ResponseKind, String> {
        let req_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let trace = TraceContext::random();
        let (tx, rx) = oneshot::channel();
        self.pending.insert(req_id, tx);

        let frame = Frame::Request {
            req_id,
            trace,
            kind,
        };
        if let Err(e) = send_frame_typed(&self.writer, frame).await {
            self.pending.remove(&req_id);
            return Err(e);
        }
        match rx.await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(remote)) => Err(format!("remote error: {remote:?}")),
            Err(_) => Err("response channel dropped before reply arrived".into()),
        }
    }

    /// Drive the inbound side of the connection: read frames, dispatch
    /// Requests to `backend`, route Notifies to `notify_handler`, and
    /// route host-admin RPCs (ReapMaterializeDir, etc.) to
    /// `admin_handler`. Returns when the reader ends or errors.
    pub async fn serve_with_reader<R>(
        &self,
        backend: Arc<dyn HostClient>,
        notify_handler: Option<Arc<dyn NotifyHandler>>,
        admin_handler: Option<Arc<dyn HostAdminHandler>>,
        mut reader: R,
    ) where
        R: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Send
            + Unpin
            + 'static,
    {
        while let Some(next) = reader.next().await {
            let msg = match next {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(error = %e, "server ws read error; ending");
                    break;
                }
            };

            if matches!(msg, Message::Close(_)) {
                tracing::debug!("server ws received Close; ending");
                break;
            }
            if matches!(msg, Message::Ping(_) | Message::Pong(_)) {
                continue;
            }

            let frame = match codec::decode(msg) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(error = %e, "server ws decode failure; skipping");
                    continue;
                }
            };

            match frame {
                Frame::Request {
                    req_id,
                    trace,
                    kind,
                } => {
                    tokio::spawn(handle_request(
                        backend.clone(),
                        admin_handler.clone(),
                        self.writer.clone(),
                        req_id,
                        trace,
                        kind,
                    ));
                }
                Frame::Notify(notify) => {
                    if let Some(handler) = notify_handler.clone() {
                        let writer = self.writer.clone();
                        tokio::spawn(async move {
                            handle_notify(handler, writer, notify).await;
                        });
                    }
                }
                Frame::Response { req_id, result } => {
                    // Demux against `pending` first — this is the
                    // reply path for host-initiated RPCs added in
                    // ADR 0007. Falls through to a warn-log if no
                    // pending entry matched (genuinely unexpected
                    // response).
                    if let Some((_, tx)) = self.pending.remove(&req_id) {
                        let _ = tx.send(result);
                    } else {
                        tracing::warn!(
                            req_id,
                            "host received Response with no pending request; ignoring",
                        );
                    }
                }
                Frame::Stream { req_id, .. } => {
                    tracing::warn!(
                        req_id,
                        "server received non-request frame from coordinator; ignoring",
                    );
                }
            }
        }
    }
}

async fn send_frame_typed(writer: &SharedSink, frame: Frame) -> Result<(), String> {
    let msg = codec::encode(&frame).map_err(|e| e.to_string())?;
    let mut w = writer.lock().await;
    w.send(msg).await.map_err(|e| e.to_string())
}

type SinkBox =
    Box<dyn futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Send + Unpin>;
type SharedSink = Arc<Mutex<SinkBox>>;

async fn send_frame(writer: &SharedSink, frame: Frame) {
    let msg = match codec::encode(&frame) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, "server failed to encode outgoing frame; dropping");
            return;
        }
    };
    let mut w = writer.lock().await;
    if let Err(e) = w.send(msg).await {
        tracing::warn!(error = %e, "server failed to write outgoing frame");
    }
}

async fn handle_notify(handler: Arc<dyn NotifyHandler>, writer: SharedSink, notify: NotifyKind) {
    match notify {
        NotifyKind::Hello {
            host_id,
            agent_version,
            wire_version,
        } => {
            if wire_version != crate::WIRE_VERSION {
                tracing::error!(
                    host_id = %host_id,
                    agent_version = %agent_version,
                    host_wire_version = wire_version,
                    coord_wire_version = crate::WIRE_VERSION,
                    "WIRE-VERSION MISMATCH — host-agent and coordinator are at incompatible wire shapes; \
                     subsequent frames will misalign. Rebuild both at the same commit, or drain before redeploy.",
                );
            }
            handler.on_hello(host_id, agent_version, wire_version).await
        }
        NotifyKind::Heartbeat(hb) => {
            let ack = handler.on_heartbeat(hb).await;
            send_frame(&writer, Frame::Notify(NotifyKind::HeartbeatAck(ack))).await;
        }
        NotifyKind::HeartbeatAck(_) => {
            // Coordinator-side ACKs flow the other direction; if a
            // host-targeted server got one it's a confused peer.
            tracing::debug!("server received unexpected HeartbeatAck; ignoring");
        }
        NotifyKind::HarnessEvent { .. } => {
            // Host → coord frame; if a host's server end got one
            // back, it's a confused peer reflecting its own
            // outbound. Ignore.
            tracing::debug!("server received unexpected HarnessEvent; ignoring");
        }
        NotifyKind::SessionEgressPolicy(_) => {
            // Coordinator → host frame; if a host's server end of
            // the WS got one back, it's a confused peer reflecting
            // its own outbound. Ignore.
            tracing::debug!("server received unexpected SessionEgressPolicy; ignoring");
        }
    }
}

async fn handle_request(
    backend: Arc<dyn HostClient>,
    admin_handler: Option<Arc<dyn HostAdminHandler>>,
    writer: SharedSink,
    req_id: u64,
    trace: crate::wire::TraceContext,
    kind: RequestKind,
) {
    // Open a span around the dispatch so every log line emitted by
    // backend code (the local SandboxBackend impl) carries the
    // coordinator's trace_id / span_id. With a real OTel exporter
    // wired in later these become a single distributed trace.
    let span = tracing::info_span!(
        "host.handle_request",
        req_id,
        trace_id = %trace.trace_id_hex(),
        span_id = %trace.span_id_hex(),
        kind = request_kind_name(&kind),
    );
    let _enter = span.enter();
    let result: Result<ResponseKind, RemoteError> = match kind {
        RequestKind::CreateSandbox { spec } => match backend.create(spec).await {
            Ok(sandbox_id) => Ok(ResponseKind::SandboxCreated { sandbox_id }),
            Err(e) => Err(RemoteError::from_sandbox(e)),
        },
        RequestKind::DestroySandbox { sandbox_id } => match backend.destroy(sandbox_id).await {
            Ok(()) => Ok(ResponseKind::Destroyed),
            Err(e) => Err(RemoteError::from_sandbox(e)),
        },
        RequestKind::ListSandboxes => match backend.list().await {
            Ok(ids) => Ok(ResponseKind::Sandboxes { ids }),
            Err(e) => Err(RemoteError::from_sandbox(e)),
        },
        // ADR 0007 Phase 6: backend chooses its own staging dir.
        // No more dest_path coercion through PathBuf, no
        // create_dir_all dance — the backend's snapshot_path_for
        // returns a stable per-snapshot dir it owns.
        RequestKind::Snapshot { sandbox_id } => match backend.snapshot(sandbox_id).await {
            Ok(metadata) => Ok(ResponseKind::Snapshotted { metadata }),
            Err(e) => Err(RemoteError::from_sandbox(e)),
        },
        // ADR 0007 Phase 6: restore by metadata. The backend
        // resolves its own local staging dir via
        // snapshot_path_for(metadata.id); PooledBackend's wrapper
        // materialises memory.bin from chunks if missing.
        RequestKind::Restore { metadata } => match backend.restore(metadata).await {
            Ok(sandbox_id) => Ok(ResponseKind::Restored { sandbox_id }),
            Err(e) => Err(RemoteError::from_sandbox(e)),
        },
        RequestKind::ExecStart {
            sandbox_id,
            request,
        } => {
            let req = request.into_engine();
            match backend.exec_stream(sandbox_id, req).await {
                Ok(stream) => {
                    // First frame: ack with assigned exec_id.
                    let exec_id = stream.exec_id.clone();
                    send_frame(
                        &writer,
                        Frame::Response {
                            req_id,
                            result: Ok(ResponseKind::ExecStarted {
                                exec_id: exec_id.clone(),
                            }),
                        },
                    )
                    .await;
                    drain_exec_stream(writer.clone(), req_id, stream.events).await;
                    return;
                }
                Err(e) => Err(RemoteError::from_sandbox(e)),
            }
        }
        RequestKind::ResolveRegistryAuth { .. } => {
            // Host → coord direction; if a host's serve loop gets
            // one back it's the coord echoing in confusion. Refuse.
            Err(RemoteError::Other(
                "ResolveRegistryAuth is host-initiated; hosts don't serve it".into(),
            ))
        }
        RequestKind::ReapMaterializeDir {
            min_age_secs,
            live_disk_manifest_ids,
        } => match admin_handler.as_ref() {
            Some(h) => match h
                .reap_materialize_dir(min_age_secs, live_disk_manifest_ids)
                .await
            {
                Ok(stats) => Ok(ResponseKind::MaterializeDirReaped { stats }),
                Err(e) => Err(RemoteError::Other(format!("reap_materialize_dir: {e}"))),
            },
            None => Err(RemoteError::Other(
                "host did not register a HostAdminHandler; ReapMaterializeDir unsupported".into(),
            )),
        },
        RequestKind::StartAgent { sandbox_id, agent } => {
            match backend.start_agent(sandbox_id, agent).await {
                Ok(()) => Ok(ResponseKind::AgentStarted),
                Err(e) => Err(RemoteError::from_sandbox(e)),
            }
        }
        RequestKind::BindHarnessSession {
            session_id,
            sandbox_id,
        } => {
            backend.bind_session(session_id, sandbox_id).await;
            Ok(ResponseKind::HarnessOk)
        }
        RequestKind::UnbindHarnessSession { session_id } => {
            backend.unbind_session(session_id).await;
            Ok(ResponseKind::HarnessOk)
        }
        RequestKind::SendHarnessPrompt { sandbox_id, text } => {
            match backend.send_prompt(sandbox_id, text).await {
                Ok(()) => Ok(ResponseKind::HarnessOk),
                Err(e) => Err(RemoteError::from_sandbox(e)),
            }
        }
    };

    send_frame(&writer, Frame::Response { req_id, result }).await;
}

fn request_kind_name(kind: &RequestKind) -> &'static str {
    match kind {
        RequestKind::CreateSandbox { .. } => "create",
        RequestKind::DestroySandbox { .. } => "destroy",
        RequestKind::ListSandboxes => "list",
        RequestKind::ExecStart { .. } => "exec_start",
        RequestKind::Snapshot { .. } => "snapshot",
        RequestKind::Restore { .. } => "restore",
        RequestKind::ResolveRegistryAuth { .. } => "resolve_registry_auth",
        RequestKind::ReapMaterializeDir { .. } => "reap_materialize_dir",
        RequestKind::StartAgent { .. } => "start_agent",
        RequestKind::BindHarnessSession { .. } => "bind_harness_session",
        RequestKind::UnbindHarnessSession { .. } => "unbind_harness_session",
        RequestKind::SendHarnessPrompt { .. } => "send_harness_prompt",
    }
}

async fn drain_exec_stream(
    writer: SharedSink,
    req_id: u64,
    mut events: engram_core::types::sandbox::ExecEventStream,
) {
    while let Some(ev) = events.next().await {
        let item = match ev {
            ExecEvent::Stdout(b) => StreamItem::ExecStdout(b.to_vec()),
            ExecEvent::Stderr(b) => StreamItem::ExecStderr(b.to_vec()),
            ExecEvent::Exit(status) => {
                send_frame(
                    &writer,
                    Frame::Stream {
                        req_id,
                        item: StreamItem::ExecExit { status },
                    },
                )
                .await;
                return;
            }
        };
        send_frame(&writer, Frame::Stream { req_id, item }).await;
    }
    // Backend's stream ended without an explicit Exit — emit one with
    // unknown status so the coordinator side terminates cleanly.
    send_frame(
        &writer,
        Frame::Stream {
            req_id,
            item: StreamItem::ExecExit { status: None },
        },
    )
    .await;
}

/// Shared by the unit tests in this module and the duplex-pair fixture
/// in `engram-coordinator`. Wraps a [`HostClient`] so a test can
/// observe the calls that crossed the wire.
#[doc(hidden)]
pub struct RecordingBackend {
    inner: Arc<dyn HostClient>,
    pub created: parking_lot::Mutex<u32>,
    pub destroyed: parking_lot::Mutex<u32>,
}

impl RecordingBackend {
    pub fn new(inner: Arc<dyn HostClient>) -> Self {
        Self {
            inner,
            created: parking_lot::Mutex::new(0),
            destroyed: parking_lot::Mutex::new(0),
        }
    }
}

#[async_trait]
impl HostClient for RecordingBackend {
    async fn create(
        &self,
        spec: engram_core::types::sandbox::SandboxSpec,
    ) -> Result<engram_core::SandboxId, SandboxError> {
        *self.created.lock() += 1;
        self.inner.create(spec).await
    }

    async fn exec_stream(
        &self,
        id: engram_core::SandboxId,
        cmd: engram_core::types::sandbox::ExecRequest,
    ) -> Result<engram_core::types::sandbox::ExecStream, SandboxError> {
        self.inner.exec_stream(id, cmd).await
    }

    async fn snapshot(
        &self,
        id: engram_core::SandboxId,
    ) -> Result<engram_core::types::snapshot::SnapshotMetadata, SandboxError> {
        self.inner.snapshot(id).await
    }

    async fn restore(
        &self,
        metadata: engram_core::types::snapshot::SnapshotMetadata,
    ) -> Result<engram_core::SandboxId, SandboxError> {
        self.inner.restore(metadata).await
    }

    async fn destroy(&self, id: engram_core::SandboxId) -> Result<(), SandboxError> {
        *self.destroyed.lock() += 1;
        self.inner.destroy(id).await
    }

    async fn list(&self) -> Result<Vec<engram_core::SandboxId>, SandboxError> {
        self.inner.list().await
    }

    async fn start_agent(
        &self,
        id: engram_core::SandboxId,
        agent: engram_core::types::sandbox::AgentSpec,
    ) -> Result<(), SandboxError> {
        self.inner.start_agent(id, agent).await
    }

    async fn notify_session_policy(
        &self,
        policy: engram_core::types::egress::SessionEgressPolicy,
    ) -> Result<(), SandboxError> {
        self.inner.notify_session_policy(policy).await
    }

    async fn guest_ip(&self, id: engram_core::SandboxId) -> Option<String> {
        self.inner.guest_ip(id).await
    }

    async fn bind_session(
        &self,
        session_id: engram_core::SessionId,
        sandbox_id: engram_core::SandboxId,
    ) {
        self.inner.bind_session(session_id, sandbox_id).await
    }

    async fn unbind_session(&self, session_id: engram_core::SessionId) {
        self.inner.unbind_session(session_id).await
    }

    async fn send_prompt(
        &self,
        sandbox_id: engram_core::SandboxId,
        text: String,
    ) -> Result<(), SandboxError> {
        self.inner.send_prompt(sandbox_id, text).await
    }
}
