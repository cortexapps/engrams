//! Coordinator-side host connection.
//!
//! [`ConnectedHost`] owns the writer half of a WebSocket and a
//! background task that demultiplexes incoming frames. Each in-flight
//! request gets a unique `req_id`; the demuxer routes the matching
//! `Frame::Response` to a oneshot and any preceding `Frame::Stream`
//! items to an mpsc keyed by the same id.
//!
//! The matching [`RemoteSandboxBackend`] wraps a `ConnectedHost` and
//! implements [`SandboxBackend`] by sending one Request per trait
//! method and awaiting the response.

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use engram_core::traits::SandboxBackend;
use engram_core::types::sandbox::{ExecEvent, ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxError, SandboxId};
use futures::sink::SinkExt;
use futures::stream::{Stream, StreamExt};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;

use crate::codec;
use crate::wire::{Frame, NotifyKind, RemoteError, RequestKind, ResponseKind, StreamItem, WireExecRequest};

/// Capacity of the per-exec stream channel. Bounds memory growth when
/// the coordinator-side consumer is slow; the host's WS writer blocks
/// past this many in-flight chunks, which propagates to the in-guest
/// agent as TCP backpressure.
const STREAM_CHANNEL_CAPACITY: usize = 64;

/// Caller-facing error type for [`ConnectedHost`] operations.
#[derive(Debug)]
pub enum ConnectionError {
    /// Underlying WebSocket / IO failure.
    Transport(String),
    /// Codec error on encode or decode.
    Codec(crate::codec::CodecError),
    /// The connection was closed before the request could complete.
    Closed,
    /// The remote returned a typed sandbox error; surface it as-is.
    Remote(RemoteError),
    /// Local protocol violation (e.g. response for an unknown req_id).
    Protocol(String),
}

impl std::fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(m) => write!(f, "transport error: {m}"),
            Self::Codec(e) => write!(f, "codec error: {e}"),
            Self::Closed => write!(f, "host connection closed"),
            Self::Remote(e) => write!(f, "remote error: {e:?}"),
            Self::Protocol(m) => write!(f, "protocol error: {m}"),
        }
    }
}

impl std::error::Error for ConnectionError {}

impl From<crate::codec::CodecError> for ConnectionError {
    fn from(e: crate::codec::CodecError) -> Self {
        Self::Codec(e)
    }
}

impl From<ConnectionError> for SandboxError {
    fn from(e: ConnectionError) -> Self {
        match e {
            ConnectionError::Remote(remote) => remote.into_sandbox(),
            ConnectionError::Closed => {
                SandboxError::Vm(Box::new(StringError("host connection closed".into())))
            }
            other => SandboxError::Vm(Box::new(StringError(other.to_string()))),
        }
    }
}

#[derive(Debug)]
struct StringError(String);

impl std::fmt::Display for StringError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StringError {}

/// What an in-flight request is waiting for.
enum Pending {
    /// Unary request — single response.
    Unary(oneshot::Sender<Result<ResponseKind, RemoteError>>),
    /// Streaming request — first frame is the response (`ExecStarted`),
    /// then `Frame::Stream` items follow until `ExecExit`.
    Streaming {
        started_tx: Option<oneshot::Sender<Result<ResponseKind, RemoteError>>>,
        stream_tx: mpsc::Sender<ExecEvent>,
    },
}

/// Sink for outgoing frames. Accepts WS messages so the demuxer task
/// can also write Pongs / Close in response to incoming control frames.
type WsSink = Box<dyn futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Send + Unpin>;

#[derive(Clone)]
pub struct ConnectedHost {
    inner: Arc<Inner>,
}

struct Inner {
    next_id: AtomicU64,
    pending: DashMap<u64, Pending>,
    /// Mutex around the writer so concurrent senders serialise frames.
    writer: Mutex<WsSink>,
    /// Set when the demuxer detects EOF / error; subsequent operations
    /// fail fast instead of hanging on a oneshot that will never fire.
    closed: parking_lot::Mutex<bool>,
    /// Channel receiving inbound Notifies (Heartbeat, Hello, Ack). The
    /// coordinator-side supervisor task drains this; if it isn't drained
    /// the demuxer drops notifies on a full channel rather than block
    /// the request/response path.
    notify_tx: mpsc::Sender<NotifyKind>,
}

impl ConnectedHost {
    /// Start a new connection over the given WS halves. Returns the
    /// handle, a notify receiver (drain on the supervisor side), and
    /// a join handle for the demuxer task — drop the handle to drop
    /// the connection (the demuxer will then exit once the writer
    /// goes away).
    pub fn spawn<W, R>(
        writer: W,
        reader: R,
    ) -> (Self, mpsc::Receiver<NotifyKind>, tokio::task::JoinHandle<()>)
    where
        W: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
            + Send
            + Unpin
            + 'static,
        R: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
            + Send
            + Unpin
            + 'static,
    {
        let (notify_tx, notify_rx) = mpsc::channel(32);
        let inner = Arc::new(Inner {
            next_id: AtomicU64::new(1),
            pending: DashMap::new(),
            writer: Mutex::new(Box::new(writer)),
            closed: parking_lot::Mutex::new(false),
            notify_tx,
        });

        let demux = tokio::spawn(demux_loop(inner.clone(), reader));
        (Self { inner }, notify_rx, demux)
    }

    /// Send a unary request and await its response. Used for every
    /// non-streaming SandboxBackend method.
    async fn unary(&self, kind: RequestKind) -> Result<ResponseKind, ConnectionError> {
        if *self.inner.closed.lock() {
            return Err(ConnectionError::Closed);
        }

        let req_id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.insert(req_id, Pending::Unary(tx));

        let frame = Frame::Request { req_id, kind };
        let msg = codec::encode(&frame)?;

        // Hold the writer lock only across the send. If send fails we
        // drop the pending entry so the oneshot doesn't leak.
        let send_result = {
            let mut w = self.inner.writer.lock().await;
            w.send(msg).await
        };
        if let Err(e) = send_result {
            self.inner.pending.remove(&req_id);
            return Err(ConnectionError::Transport(e.to_string()));
        }

        match rx.await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(remote)) => Err(ConnectionError::Remote(remote)),
            Err(_) => Err(ConnectionError::Closed),
        }
    }

    /// Start a streaming exec request. Returns the assigned `exec_id`
    /// plus a receiver of [`ExecEvent`]s ending in exactly one
    /// `ExecEvent::Exit`.
    async fn exec_stream(
        &self,
        sandbox_id: SandboxId,
        request: ExecRequest,
    ) -> Result<(String, mpsc::Receiver<ExecEvent>), ConnectionError> {
        if *self.inner.closed.lock() {
            return Err(ConnectionError::Closed);
        }

        let req_id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (started_tx, started_rx) = oneshot::channel();
        let (stream_tx, stream_rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);

        self.inner.pending.insert(
            req_id,
            Pending::Streaming {
                started_tx: Some(started_tx),
                stream_tx,
            },
        );

        let frame = Frame::Request {
            req_id,
            kind: RequestKind::ExecStart {
                sandbox_id,
                request: WireExecRequest::from_engine(request),
            },
        };
        let msg = codec::encode(&frame)?;

        let send_result = {
            let mut w = self.inner.writer.lock().await;
            w.send(msg).await
        };
        if let Err(e) = send_result {
            self.inner.pending.remove(&req_id);
            return Err(ConnectionError::Transport(e.to_string()));
        }

        match started_rx.await {
            Ok(Ok(ResponseKind::ExecStarted { exec_id })) => Ok((exec_id, stream_rx)),
            Ok(Ok(other)) => Err(ConnectionError::Protocol(format!(
                "expected ExecStarted, got {other:?}"
            ))),
            Ok(Err(remote)) => {
                // Remote errored before any stream items — drop the
                // stream channel.
                self.inner.pending.remove(&req_id);
                Err(ConnectionError::Remote(remote))
            }
            Err(_) => Err(ConnectionError::Closed),
        }
    }

    /// Send a notification (Heartbeat, HeartbeatAck, Hello) — fire and
    /// forget, no response expected.
    pub async fn notify(&self, notify: NotifyKind) -> Result<(), ConnectionError> {
        if *self.inner.closed.lock() {
            return Err(ConnectionError::Closed);
        }
        let frame = Frame::Notify(notify);
        let msg = codec::encode(&frame)?;
        let mut w = self.inner.writer.lock().await;
        w.send(msg)
            .await
            .map_err(|e| ConnectionError::Transport(e.to_string()))
    }

    /// True once the demuxer has observed EOF / error on the read half.
    pub fn is_closed(&self) -> bool {
        *self.inner.closed.lock()
    }
}

async fn demux_loop<R>(inner: Arc<Inner>, mut reader: R)
where
    R: futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Send
        + Unpin
        + 'static,
{
    while let Some(next) = reader.next().await {
        let msg = match next {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "host ws read error; closing connection");
                break;
            }
        };

        // Tungstenite control frames: ignore Pong (autopong handles it),
        // exit on Close.
        if matches!(msg, Message::Close(_)) {
            tracing::debug!("host ws received Close; ending demux");
            break;
        }
        if matches!(msg, Message::Ping(_) | Message::Pong(_)) {
            continue;
        }

        let frame = match codec::decode(msg) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "host ws decode failure; skipping frame");
                continue;
            }
        };

        match frame {
            Frame::Response { req_id, result } => {
                let removed = inner.pending.remove(&req_id);
                match removed {
                    Some((_, Pending::Unary(tx))) => {
                        let _ = tx.send(result);
                    }
                    Some((_, Pending::Streaming { started_tx, stream_tx })) => {
                        // For streaming RPCs the first Response is the
                        // start ack; later Stream items + the exit
                        // close out the channel. Re-insert the streaming
                        // entry so subsequent stream items still find
                        // their channel.
                        if let Some(tx) = started_tx {
                            let _ = tx.send(result.clone());
                        }
                        if matches!(result, Ok(_)) {
                            inner.pending.insert(
                                req_id,
                                Pending::Streaming {
                                    started_tx: None,
                                    stream_tx,
                                },
                            );
                        }
                    }
                    None => {
                        tracing::warn!(req_id, "response for unknown request");
                    }
                }
            }
            Frame::Stream { req_id, item } => {
                let mut should_drop = false;
                if let Some(entry) = inner.pending.get(&req_id) {
                    if let Pending::Streaming { stream_tx, .. } = entry.value() {
                        let event = match item {
                            StreamItem::ExecStdout(b) => ExecEvent::Stdout(b.into()),
                            StreamItem::ExecStderr(b) => ExecEvent::Stderr(b.into()),
                            StreamItem::ExecExit { status } => {
                                should_drop = true;
                                ExecEvent::Exit(status)
                            }
                        };
                        let _ = stream_tx.send(event).await;
                    }
                }
                if should_drop {
                    inner.pending.remove(&req_id);
                }
            }
            Frame::Notify(notify) => {
                // Forward to the supervisor task. `try_send` so a slow
                // supervisor doesn't block the demuxer (and stall every
                // outstanding Request). Notify backlog past channel
                // capacity gets dropped — this is acceptable because
                // each Notify is independent and the next heartbeat
                // arrives within seconds.
                if let Err(e) = inner.notify_tx.try_send(notify) {
                    tracing::warn!(error = %e, "notify channel full or closed; dropping");
                }
            }
            Frame::Request { req_id, .. } => {
                tracing::warn!(
                    req_id,
                    "client received Request frame from host; ignoring"
                );
            }
        }
    }

    // Mark closed and fail any in-flight requests.
    *inner.closed.lock() = true;
    let pending: Vec<u64> = inner.pending.iter().map(|e| *e.key()).collect();
    for req_id in pending {
        if let Some((_, p)) = inner.pending.remove(&req_id) {
            match p {
                Pending::Unary(tx) => {
                    let _ = tx.send(Err(RemoteError::Other("connection closed".into())));
                }
                Pending::Streaming {
                    started_tx,
                    stream_tx,
                } => {
                    if let Some(tx) = started_tx {
                        let _ = tx.send(Err(RemoteError::Other("connection closed".into())));
                    }
                    drop(stream_tx);
                }
            }
        }
    }
}

/// `SandboxBackend` impl that forwards every method to a [`ConnectedHost`].
/// This is what the coordinator stores per registered host so existing
/// call sites keep working without rewrites.
pub struct RemoteSandboxBackend {
    host: ConnectedHost,
}

impl RemoteSandboxBackend {
    pub fn new(host: ConnectedHost) -> Self {
        Self { host }
    }
}

#[async_trait]
impl SandboxBackend for RemoteSandboxBackend {
    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        match self.host.unary(RequestKind::CreateSandbox { spec }).await {
            Ok(ResponseKind::SandboxCreated { sandbox_id }) => Ok(sandbox_id),
            Ok(other) => Err(SandboxError::Vm(Box::new(StringError(format!(
                "unexpected response: {other:?}"
            ))))),
            Err(e) => Err(e.into()),
        }
    }

    async fn exec_stream(
        &self,
        id: SandboxId,
        cmd: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        let (exec_id, mut rx) = self
            .host
            .exec_stream(id, cmd)
            .await
            .map_err(SandboxError::from)?;

        let stream = async_stream::stream! {
            while let Some(ev) = rx.recv().await {
                let terminal = ev.is_terminal();
                yield ev;
                if terminal {
                    break;
                }
            }
        };

        Ok(ExecStream {
            sandbox_id: id,
            exec_id,
            events: Box::pin(stream) as Pin<Box<dyn Stream<Item = ExecEvent> + Send + 'static>>,
        })
    }

    async fn snapshot(
        &self,
        id: SandboxId,
        dest: &std::path::Path,
    ) -> Result<SnapshotMetadata, SandboxError> {
        let dest_str = dest.to_string_lossy().into_owned();
        match self
            .host
            .unary(RequestKind::Snapshot {
                sandbox_id: id,
                dest_path: dest_str,
            })
            .await
        {
            Ok(ResponseKind::Snapshotted { metadata }) => Ok(metadata),
            Ok(other) => Err(SandboxError::Vm(Box::new(StringError(format!(
                "unexpected response: {other:?}"
            ))))),
            Err(e) => Err(e.into()),
        }
    }

    async fn restore(&self, src: std::path::PathBuf) -> Result<SandboxId, SandboxError> {
        let src_str = src.to_string_lossy().into_owned();
        match self
            .host
            .unary(RequestKind::Restore { src_path: src_str })
            .await
        {
            Ok(ResponseKind::Restored { sandbox_id }) => Ok(sandbox_id),
            Ok(other) => Err(SandboxError::Vm(Box::new(StringError(format!(
                "unexpected response: {other:?}"
            ))))),
            Err(e) => Err(e.into()),
        }
    }

    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        match self
            .host
            .unary(RequestKind::DestroySandbox { sandbox_id: id })
            .await
        {
            Ok(ResponseKind::Destroyed) => Ok(()),
            Ok(other) => Err(SandboxError::Vm(Box::new(StringError(format!(
                "unexpected response: {other:?}"
            ))))),
            Err(e) => Err(e.into()),
        }
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        match self.host.unary(RequestKind::ListSandboxes).await {
            Ok(ResponseKind::Sandboxes { ids }) => Ok(ids),
            Ok(other) => Err(SandboxError::Vm(Box::new(StringError(format!(
                "unexpected response: {other:?}"
            ))))),
            Err(e) => Err(e.into()),
        }
    }
}
