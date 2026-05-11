//! Host-side WebSocket server: dispatches incoming frames to a local
//! [`SandboxBackend`] and streams responses back to the coordinator.
//!
//! The host is the *server* of the SandboxBackend RPC even though it
//! dialled the coordinator — once the WebSocket is up, frames flow
//! both ways. The host originates only Notifies (Hello, Heartbeat);
//! all Requests come from the coordinator.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use engram_core::traits::SandboxBackend;
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
    /// view of capacity / warm-pool / local-snapshot state.
    async fn on_heartbeat(&self, hb: Heartbeat) -> HeartbeatAck;
}

/// Server side: read frames from a WS connection and dispatch them.
///
/// `backend` services Request frames; `notify_handler` is called for
/// inbound Notifies. Returns when the read half ends or errors.
pub async fn serve<W, R>(
    backend: Arc<dyn SandboxBackend>,
    notify_handler: Option<Arc<dyn NotifyHandler>>,
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
        .serve_with_reader(backend, notify_handler, reader)
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
    /// Requests to `backend`, route Notifies to `notify_handler`.
    /// Returns when the reader ends or errors.
    pub async fn serve_with_reader<R>(
        &self,
        backend: Arc<dyn SandboxBackend>,
        notify_handler: Option<Arc<dyn NotifyHandler>>,
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
        NotifyKind::SessionEgressPolicy(_) => {
            // Coordinator → host frame; if a host's server end of
            // the WS got one back, it's a confused peer reflecting
            // its own outbound. Ignore.
            tracing::debug!("server received unexpected SessionEgressPolicy; ignoring");
        }
    }
}

async fn handle_request(
    backend: Arc<dyn SandboxBackend>,
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
        RequestKind::Snapshot {
            sandbox_id,
            dest_path,
        } => {
            let path = PathBuf::from(dest_path);
            // Ensure the destination dir exists — backends generally
            // assume it's preallocated. Single-host (mode=all) creates
            // it on the coordinator side already; explicit hosts may
            // be running on a different filesystem so we re-create here.
            if let Err(e) = tokio::fs::create_dir_all(&path).await {
                send_frame(
                    &writer,
                    Frame::Response {
                        req_id,
                        result: Err(RemoteError::Io(e.to_string())),
                    },
                )
                .await;
                return;
            }
            match backend.snapshot(sandbox_id, &path).await {
                Ok(metadata) => Ok(ResponseKind::Snapshotted { metadata }),
                Err(e) => Err(RemoteError::from_sandbox(e)),
            }
        }
        RequestKind::Restore { src_path } => match backend.restore(PathBuf::from(src_path)).await {
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
/// in `engram-coordinator`. Wraps a [`SandboxBackend`] so a test can
/// observe the calls that crossed the wire.
#[doc(hidden)]
pub struct RecordingBackend {
    inner: Arc<dyn SandboxBackend>,
    pub created: parking_lot::Mutex<u32>,
    pub destroyed: parking_lot::Mutex<u32>,
}

impl RecordingBackend {
    pub fn new(inner: Arc<dyn SandboxBackend>) -> Self {
        Self {
            inner,
            created: parking_lot::Mutex::new(0),
            destroyed: parking_lot::Mutex::new(0),
        }
    }
}

#[async_trait]
impl SandboxBackend for RecordingBackend {
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
        dest: &std::path::Path,
    ) -> Result<engram_core::types::snapshot::SnapshotMetadata, SandboxError> {
        self.inner.snapshot(id, dest).await
    }

    async fn restore(&self, src: PathBuf) -> Result<engram_core::SandboxId, SandboxError> {
        self.inner.restore(src).await
    }

    async fn destroy(&self, id: engram_core::SandboxId) -> Result<(), SandboxError> {
        *self.destroyed.lock() += 1;
        self.inner.destroy(id).await
    }

    async fn list(&self) -> Result<Vec<engram_core::SandboxId>, SandboxError> {
        self.inner.list().await
    }
}
