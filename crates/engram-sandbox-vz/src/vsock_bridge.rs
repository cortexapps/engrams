//! Bridge between Apple's `VZVirtioSocketDevice` (real virtio-vsock)
//! and the surfaces the rest of the codebase expects — the UDS-shaped
//! agentd channel `engram-sandbox-firecracker` exposes, the harness /
//! upload sinks, and the ADR 0066 port relay's `open_guest_stream`.
//!
//! # Why real vsock (ADR 0066 Phase 2)
//!
//! VZ used to bridge these channels over a multi-port
//! `VZVirtioConsoleDeviceConfiguration`, which gives **one byte stream
//! per port**: concurrent connections on a port serialise behind a
//! mutex. For the port relay that means one persistent forwarded
//! connection (an HMR WebSocket, a noVNC stream) would starve every
//! other forwarded connection — head-of-line blocking. `VZVirtioSocketDevice`
//! muxes any number of concurrent streams per port (like Firecracker's
//! virtio-vsock), so each forwarded connection is independent. The Kata
//! guest kernel VZ boots ships `CONFIG_VIRTIO_VSOCKETS=y` built-in, so
//! the constraint that motivated the console swap no longer applies.
//!
//! # Ports & directions
//!
//! ```text
//!   port 1024 (host → guest, agentd):  UDS at <base>_1024 ←→ connectToPort(1024)
//!   port 1026 (guest → host, harness): VZVirtioSocketListener → harness_sink
//!   port 1027 (guest → host, ready):   VZVirtioSocketListener → drained
//!   port 1029 (guest → host, upload):  VZVirtioSocketListener → upload_sink
//!   port 1030 (host → guest, relay):   connectToPort(1030) via open_guest_stream
//! ```
//!
//! - **Agentd (1024, host→guest):** we bind a host `UnixListener` and,
//!   per accept, `connectToPort(1024)` inside the guest and splice —
//!   preserving the `UnixStream::connect(<base>_1024)` surface the
//!   backend's `start_agent` / `exec_stream` / `start_shell` / `guest_endpoints`
//!   already use. Each UDS accept is its own vsock stream, so concurrent
//!   control RPCs no longer serialise (unlike the console bridge).
//! - **Harness (1026) / upload (1029), guest→host:** register a
//!   `VZVirtioSocketListener` per port; each guest dial becomes its own
//!   `VZVirtioSocketConnection`, whose fd we hand straight to the sink as
//!   a `HarnessByteStream`. Upload is now one vsock connection per
//!   upload — exactly like FC — so the console bridge's per-stream
//!   `upload_pump` serialisation is retired.
//! - **Ready (1027), guest→host:** agentd (on the vsock transport) dials
//!   the readiness port at startup and writes one `AgentReady` frame
//!   (fire-and-forget). We register a listener that drains and drops it,
//!   so the guest's handshake completes on the first dial instead of
//!   spinning ~90s. VZ keeps its existing "first read blocks until agentd
//!   binds" readiness model; the drain just prevents the boot stall.
//! - **Relay (1030), host→guest:** served directly by the backend's
//!   `open_guest_stream` through the shared [`VsockConnector`] — no
//!   bridge-side listener, one fresh vsock stream per forwarded browser
//!   connection (the ADR 0066 no-HOL invariant).
//!
//! # Cleanup
//!
//! [`VsockBridge::stop`] aborts every pump/consumer task, unregisters
//! the guest-initiated listeners (so a subsequent restore doesn't
//! collide), and removes the host-initiated UDS files. `Drop` aborts
//! tasks as a backstop.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use engram_core::traits::sandbox::{HarnessByteStream, HarnessSink, UploadSink};
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_foundation::NSError;
use objc2_virtualization::{
    VZSocketDevice, VZVirtioSocketConnection, VZVirtioSocketDevice, VZVirtioSocketListener,
    VZVirtualMachine,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::vm::Sendable;

/// Vsock ports — same well-known values as `engram-sandbox-firecracker`
/// since the in-guest agentd / harness / share binaries listen on the
/// same numbers regardless of which VMM is hosting the VM.
const VSOCK_PORT_AGENTD: u32 = 1024;
const VSOCK_PORT_HARNESS: u32 = 1026;
/// Readiness handshake port (guest→host). agentd on the vsock transport
/// dials this once at startup (`engram_agentd::ENGRAM_AGENTD_READY_PORT`).
const VSOCK_PORT_READY: u32 = 1027;
/// ADR 0026 artifact upload (`engram_harness_proto::UPLOAD_VSOCK_PORT`).
const VSOCK_PORT_UPLOAD: u32 = 1029;

/// How long a host→guest dial (`connectToPort`) retries a boot-race /
/// post-restore reset before giving up. Sized to cover kernel boot →
/// init → agentd binding its vsock listener; mirrors FC's
/// `connect_fc_vsock` retry budget.
const DIAL_RETRY_BUDGET_SECS: u64 = 15;

/// Per-direction splice buffer for the agentd UDS pump. 256 KiB ≥
/// vsock's ~64 KiB per-connection credit window.
const COPY_BUF: usize = 256 * 1024;

/// Errors from the bridge layer.
#[derive(Debug)]
pub(crate) enum BridgeError {
    /// The live VM doesn't expose a `VZVirtioSocketDevice` — should be
    /// impossible since `vm::build_configuration` always attaches one,
    /// but worth a clean error if Apple ever changes that contract.
    NoSocketDevice,
    /// Couldn't bind the host-side UDS for the agentd port. Most often
    /// `EADDRINUSE` from a stale file.
    Bind(PathBuf, std::io::Error),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSocketDevice => {
                write!(f, "vz vm has no VZVirtioSocketDevice — bridge cannot run")
            }
            Self::Bind(p, e) => write!(f, "vz vsock bind {} failed: {e}", p.display()),
        }
    }
}

impl std::error::Error for BridgeError {}

impl From<BridgeError> for engram_core::SandboxError {
    fn from(e: BridgeError) -> Self {
        engram_core::SandboxError::Vm(Box::new(e))
    }
}

// ---- VsockConnector ---------------------------------------------------

/// Cloneable handle that dials host→guest vsock ports on one VM. Shared
/// by the agentd UDS pump (1024) and the backend's `open_guest_stream`
/// (the port relay, 1030). Every VZ device call runs on the VM's serial
/// dispatch queue, per Apple's threading contract.
#[derive(Clone)]
pub(crate) struct VsockConnector {
    device: Sendable<Retained<VZVirtioSocketDevice>>,
    queue: DispatchRetained<DispatchQueue>,
}

impl VsockConnector {
    /// Dial `port` inside the guest, retrying a boot-race / post-restore
    /// reset within [`DIAL_RETRY_BUDGET_SECS`], and return the connection
    /// as an owned `AsyncRead + AsyncWrite` stream. Used for `open_guest_stream`.
    pub(crate) async fn connect_stream(&self, port: u32) -> std::io::Result<HarnessByteStream> {
        let fd = self
            .dial_with_retry(port)
            .await
            .map_err(std::io::Error::other)?;
        let stream = AsyncRawFdStream::new(fd)?;
        Ok(Box::pin(stream))
    }

    /// Retry [`Self::dial_once`] with backoff while the kernel is booting
    /// and the in-guest listener hasn't bound yet. "Connection reset by
    /// peer" is the typical boot signal — VZ RSTs the host because no
    /// guest process is listening on the port yet.
    async fn dial_with_retry(&self, port: u32) -> Result<OwnedFd, String> {
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(DIAL_RETRY_BUDGET_SECS);
        let mut backoff = std::time::Duration::from_millis(100);
        loop {
            match self.dial_once(port).await {
                Ok(fd) => return Ok(fd),
                Err(e) => {
                    let is_boot_race = e.contains("Connection reset by peer")
                        || e.contains("connection refused")
                        || e.contains("connection was refused")
                        || e.contains("Socket is not connected");
                    if !is_boot_race || tokio::time::Instant::now() >= deadline {
                        return Err(e);
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(std::time::Duration::from_secs(1));
                }
            }
        }
    }

    /// One `connectToPort:completionHandler:` attempt. Bridges the
    /// completion-handler callback to a tokio oneshot; returns the
    /// connection's fd `dup`'d into an `OwnedFd` we own independently of
    /// the (dropped) `VZVirtioSocketConnection`.
    async fn dial_once(&self, port: u32) -> Result<OwnedFd, String> {
        let (tx, rx) = oneshot::channel::<Result<OwnedFd, String>>();
        let device = self.device.clone();
        let tx = Mutex::new(Some(tx));
        let block = RcBlock::new(
            move |conn: *mut VZVirtioSocketConnection, err: *mut NSError| {
                let result = connection_ptr_to_fd(conn, err);
                if let Some(tx) = tx.lock().expect("oneshot mutex poisoned").take() {
                    let _ = tx.send(result);
                }
            },
        );
        let block = Sendable(block);
        self.queue.exec_async(move || {
            // SAFETY: dispatched onto the VM's serial queue, so the
            // device call is on a VZ-safe thread; `block` outlives the
            // call (owned by the queue until the handler fires).
            unsafe {
                device.connectToPort_completionHandler(port, &block);
            }
        });
        rx.await
            .map_err(|_| "vz dial: completion channel dropped".to_string())?
    }
}

/// Turn the `(connection, error)` a `connectToPort` completion handler
/// receives into an owned fd. `dup`s the connection's fd so we own it
/// independently of the `VZVirtioSocketConnection` (which we drop).
fn connection_ptr_to_fd(
    conn: *mut VZVirtioSocketConnection,
    err: *mut NSError,
) -> Result<OwnedFd, String> {
    if !err.is_null() {
        // SAFETY: VZ guarantees `err` is a valid retained NSError when non-null.
        let err = unsafe { Retained::retain(err) };
        return Err(err
            .map(|e| e.localizedDescription().to_string())
            .unwrap_or_else(|| "<nil error>".into()));
    }
    if conn.is_null() {
        return Err("VZ returned nil connection with nil error".into());
    }
    // SAFETY: VZ hands us a retained-on-our-behalf connection; re-retain
    // to own it for the duration of this function.
    let conn = unsafe { Retained::retain(conn) };
    let Some(conn) = conn else {
        return Err("VZVirtioSocketConnection retain returned None".into());
    };
    // SAFETY: `fileDescriptor` returns the connection's socket fd; valid
    // while `conn` is retained.
    let raw = unsafe { conn.fileDescriptor() };
    if raw < 0 {
        return Err("VZVirtioSocketConnection.fileDescriptor returned -1".into());
    }
    // SAFETY: dup'ing a valid fd is sound; we own the dup (closed via
    // OwnedFd::drop). `conn` closes its own copy when it drops below.
    let dup = unsafe { libc::dup(raw) };
    if dup < 0 {
        return Err(format!(
            "dup VZVirtioSocketConnection fd failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `dup` returned a fresh valid fd we now own.
    Ok(unsafe { OwnedFd::from_raw_fd(dup) })
}

// ---- VsockBridge ------------------------------------------------------

/// A bound vsock bridge for one VM. Owns the spawned pump/consumer tasks
/// plus the socket-device handle (for `removeSocketListenerForPort` at
/// cleanup — the VM keeps its own retain).
pub(crate) struct VsockBridge {
    base_path: PathBuf,
    /// Aborted by `stop` and `Drop`.
    tasks: Vec<JoinHandle<()>>,
    /// Device + queue, held so `stop` can unregister the guest listeners.
    device: Sendable<Retained<VZVirtioSocketDevice>>,
    queue: DispatchRetained<DispatchQueue>,
    /// Ports we registered a `VZVirtioSocketListener` on (unregistered on stop).
    registered_listener_ports: Vec<u32>,
    /// The guest-initiated listeners and their delegates, held alive for the
    /// bridge's lifetime. `VZVirtioSocketListener::setDelegate:` is a **weak**
    /// property, so if we let the delegate drop after registration it
    /// deallocates, the listener's delegate ref goes nil, and
    /// `should_accept` is never called — every guest-initiated connection
    /// (ready 1027, harness 1026, upload 1029) is then silently rejected and
    /// agentd spins its full ready-port deadline. Keeping both here is what
    /// keeps the accept path live.
    _listeners: Vec<Sendable<Retained<VZVirtioSocketListener>>>,
    _delegates: Vec<Sendable<Retained<VsockListenerDelegate>>>,
}

impl VsockBridge {
    /// Look up the VM's `VZVirtioSocketDevice`, bind the agentd UDS
    /// (1024) host→guest pump, and register the guest→host listeners
    /// (harness 1026 → `harness_sink`, upload 1029 → `upload_sink`, ready
    /// 1027 → drain). The VM must already be `start()`ed — VZ socket-device
    /// APIs only operate on a running machine.
    ///
    /// Returns the bridge alongside a [`VsockConnector`] the backend keeps
    /// for `open_guest_stream` (the port relay, 1030).
    pub async fn start(
        vm: Sendable<Retained<VZVirtualMachine>>,
        queue: DispatchRetained<DispatchQueue>,
        base_path: PathBuf,
        harness_sink: Option<HarnessSink>,
        upload_sink: Option<UploadSink>,
    ) -> Result<(Self, VsockConnector), BridgeError> {
        let socket_device = lookup_socket_device(&queue, &vm)
            .await
            .ok_or(BridgeError::NoSocketDevice)?;
        let device = Sendable(socket_device);
        let connector = VsockConnector {
            device: device.clone(),
            queue: queue.clone(),
        };

        let mut tasks: Vec<JoinHandle<()>> = Vec::new();
        let mut registered_listener_ports: Vec<u32> = Vec::new();
        // Held alive for the bridge's lifetime — see the struct field docs
        // (`setDelegate:` is weak; dropping these silently kills the accept path).
        let mut kept_listeners: Vec<Sendable<Retained<VZVirtioSocketListener>>> = Vec::new();
        let mut kept_delegates: Vec<Sendable<Retained<VsockListenerDelegate>>> = Vec::new();

        // Host-initiated agentd port: bind the UDS first so the backend's
        // start_agent / exec_stream dial doesn't race a not-yet-bound
        // window between create() returning and the bridge going live.
        {
            let uds_path = port_uds_path(&base_path, VSOCK_PORT_AGENTD);
            let _ = tokio::fs::remove_file(&uds_path).await; // tolerate stale
            let listener = UnixListener::bind(&uds_path)
                .map_err(|e| BridgeError::Bind(uds_path.clone(), e))?;
            tracing::debug!(port = VSOCK_PORT_AGENTD, path = %uds_path.display(), "vz vsock host-initiated bound");
            tasks.push(tokio::spawn(agentd_uds_pump(
                listener,
                connector.clone(),
                VSOCK_PORT_AGENTD,
            )));
        }

        // Guest-initiated listeners. Each accepted connection's fd is
        // handed straight to the port's sink as a HarnessByteStream — no
        // duplex hop, no per-connection serialisation.
        let harness_delivery: Option<ConnDelivery> = harness_sink.map(harness_delivery);
        let upload_delivery: Option<ConnDelivery> = upload_sink.map(upload_delivery);
        let ready_delivery: Option<ConnDelivery> = Some(ready_drain_delivery());

        for (port, delivery) in [
            (VSOCK_PORT_HARNESS, harness_delivery),
            (VSOCK_PORT_UPLOAD, upload_delivery),
            (VSOCK_PORT_READY, ready_delivery),
        ] {
            let Some(delivery) = delivery else {
                tracing::warn!(
                    port,
                    "no sink registered for guest-initiated port; skipping"
                );
                continue;
            };
            let (conn_tx, conn_rx) =
                mpsc::unbounded_channel::<Sendable<Retained<VZVirtioSocketConnection>>>();
            let delegate = VsockListenerDelegate::new(conn_tx);
            // SAFETY: VZVirtioSocketListener::new returns a freshly
            // retained, non-nil instance; we attach our delegate before
            // handing it to the device. `setDelegate:` is a WEAK property, so
            // we pass a clone and keep our own retained ref in `kept_delegates`
            // below — otherwise the delegate deallocates and the listener stops
            // accepting.
            let listener: Retained<VZVirtioSocketListener> = unsafe {
                let listener = VZVirtioSocketListener::new();
                listener.setDelegate(Some(&objc2::runtime::ProtocolObject::from_retained(
                    delegate.clone(),
                )));
                listener
            };
            // Wrap in `Sendable` before the `.await` below so no raw ObjC
            // `Retained` (non-Send) crosses the await boundary.
            let listener = Sendable(listener);
            let delegate = Sendable(delegate);
            // setSocketListener:forPort: must run on the VM's queue.
            run_on_queue(&queue, {
                let device = device.clone();
                let listener = listener.clone();
                move || unsafe {
                    device.setSocketListener_forPort(&listener, port);
                }
            })
            .await;
            registered_listener_ports.push(port);
            kept_listeners.push(listener);
            kept_delegates.push(delegate);
            tasks.push(tokio::spawn(guest_listener_consumer(conn_rx, delivery)));
        }

        Ok((
            Self {
                base_path,
                tasks,
                device,
                queue,
                registered_listener_ports,
                _listeners: kept_listeners,
                _delegates: kept_delegates,
            },
            connector,
        ))
    }

    /// Tear down: abort pump/consumer tasks, unregister the guest-initiated
    /// listeners, and remove the agentd UDS file.
    pub async fn stop(&mut self) {
        for h in self.tasks.drain(..) {
            h.abort();
        }
        for &port in &std::mem::take(&mut self.registered_listener_ports) {
            let device = self.device.clone();
            run_on_queue(&self.queue, move || unsafe {
                device.removeSocketListenerForPort(port);
            })
            .await;
        }
        let _ = tokio::fs::remove_file(port_uds_path(&self.base_path, VSOCK_PORT_AGENTD)).await;
    }
}

impl Drop for VsockBridge {
    fn drop(&mut self) {
        // Best-effort task abort — async cleanup (listener unregister +
        // UDS unlink) is `stop`'s job; this only fires on panic paths.
        for h in self.tasks.drain(..) {
            h.abort();
        }
    }
}

pub(crate) fn port_uds_path(base: &Path, port: u32) -> PathBuf {
    let mut p = base.to_path_buf();
    let mut name = p.file_name().unwrap_or_default().to_owned();
    name.push(format!("_{port}"));
    p.set_file_name(name);
    p
}

/// Find the first `VZVirtioSocketDevice` on a running VM, on its queue.
/// `vm::build_configuration` attaches exactly one, so `[0]` is ours.
async fn lookup_socket_device(
    queue: &DispatchQueue,
    vm: &Sendable<Retained<VZVirtualMachine>>,
) -> Option<Retained<VZVirtioSocketDevice>> {
    let (tx, rx) = oneshot::channel::<Option<Sendable<Retained<VZVirtioSocketDevice>>>>();
    let vm = vm.clone();
    let tx = Mutex::new(Some(tx));
    queue.exec_async(move || {
        // SAFETY: dispatched onto the VM's queue, so reaching into the
        // socketDevices array is sound. The retained NSArray + elements
        // live for the closure.
        let result = unsafe {
            let array = vm.socketDevices();
            if array.count() == 0 {
                None
            } else {
                let any: Retained<VZSocketDevice> = array.objectAtIndex(0);
                any.downcast::<VZVirtioSocketDevice>().ok().map(Sendable)
            }
        };
        if let Some(tx) = tx.lock().expect("oneshot mutex poisoned").take() {
            let _ = tx.send(result);
        }
    });
    let device = rx.await.ok()??;
    Some(device.0)
}

// ---- Host-initiated agentd pump --------------------------------------

/// Accept on the host UDS listener and, per accept, dial the guest's
/// agentd port and splice bytes. Each accepted connection gets its own
/// vsock stream + task, so concurrent control RPCs (exec, start_shell,
/// guest_endpoints) don't serialise.
async fn agentd_uds_pump(listener: UnixListener, connector: VsockConnector, port: u32) {
    loop {
        let (host_stream, _peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, port, "vz vsock accept failed; pump exiting");
                return;
            }
        };
        let connector = connector.clone();
        tokio::spawn(async move {
            match connector.connect_stream(port).await {
                Ok(guest) => {
                    if let Err(e) = pump(guest, host_stream).await {
                        tracing::warn!(error = %e, port, "vz vsock pump ended with error");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, port, "vz vsock connectToPort failed after retry budget");
                }
            }
        });
    }
}

/// Bidirectional copy between a guest vsock stream and a host UnixStream.
/// Returns when either side EOFs or errors.
async fn pump(guest: HarnessByteStream, host_stream: UnixStream) -> std::io::Result<()> {
    let (mut g_r, mut g_w) = tokio::io::split(guest);
    let (mut h_r, mut h_w) = host_stream.into_split();
    let h_to_g = async move {
        let _ = copy_sized(&mut h_r, &mut g_w).await;
        let _ = g_w.shutdown().await;
    };
    let g_to_h = async move {
        let _ = copy_sized(&mut g_r, &mut h_w).await;
        let _ = h_w.shutdown().await;
    };
    tokio::select! {
        _ = h_to_g => {}
        _ = g_to_h => {}
    }
    Ok(())
}

/// Copy `r` → `w` with a [`COPY_BUF`]-sized buffer (≥ vsock's per-connection
/// credit window). `copy_bidirectional_with_sizes` isn't ergonomic across a
/// split stream, so we size a manual loop.
async fn copy_sized<R, W>(r: &mut R, w: &mut W) -> std::io::Result<u64>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut buf = vec![0u8; COPY_BUF];
    let mut total = 0u64;
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            return Ok(total);
        }
        w.write_all(&buf[..n]).await?;
        total += n as u64;
    }
}

// ---- Guest-initiated listener delivery -------------------------------

/// Per-port delivery closure: turns each accepted `VZVirtioSocketConnection`
/// (already `dup`'d into an `OwnedFd`) into a stream and routes it. Boxed
/// so harness / upload / ready all share one consumer shape.
type ConnDelivery = Arc<dyn Fn(OwnedFd) + Send + Sync>;

/// Deliver each guest connection straight to the harness sink as a
/// `HarnessByteStream`.
fn harness_delivery(sink: HarnessSink) -> ConnDelivery {
    Arc::new(move |fd: OwnedFd| match AsyncRawFdStream::new(fd) {
        Ok(s) => sink(Box::pin(s)),
        Err(e) => tracing::warn!(error = %e, "vz harness conn: AsyncRawFdStream::new failed"),
    })
}

/// Deliver each guest connection straight to the upload sink. One vsock
/// connection per upload (like FC), so the sink drives the whole
/// request/response over this stream — no cross-upload serialisation.
fn upload_delivery(sink: UploadSink) -> ConnDelivery {
    Arc::new(move |fd: OwnedFd| match AsyncRawFdStream::new(fd) {
        Ok(s) => sink(Box::pin(s)),
        Err(e) => tracing::warn!(error = %e, "vz upload conn: AsyncRawFdStream::new failed"),
    })
}

/// Drain the readiness handshake: agentd writes one `AgentReady` frame
/// and half-closes. Read to EOF and drop so the guest's dial completes
/// on the first attempt (no ~90s spin) without gating anything.
fn ready_drain_delivery() -> ConnDelivery {
    Arc::new(move |fd: OwnedFd| {
        let stream = match AsyncRawFdStream::new(fd) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "vz ready conn: AsyncRawFdStream::new failed");
                return;
            }
        };
        tokio::spawn(async move {
            let mut stream = stream;
            let mut sink = tokio::io::sink();
            let _ = tokio::io::copy(&mut stream, &mut sink).await;
            tracing::debug!("vz ready-port handshake drained");
        });
    })
}

/// Consume each connection the delegate accepts on a guest-initiated
/// port: `dup` its fd and hand it to the port's [`ConnDelivery`].
async fn guest_listener_consumer(
    mut conn_rx: mpsc::UnboundedReceiver<Sendable<Retained<VZVirtioSocketConnection>>>,
    delivery: ConnDelivery,
) {
    while let Some(conn) = conn_rx.recv().await {
        // SAFETY: `conn` is a valid retained connection; `fileDescriptor`
        // is its socket fd, valid while retained.
        let raw = unsafe { conn.fileDescriptor() };
        if raw < 0 {
            tracing::warn!("vz guest conn returned -1 fd; dropping");
            continue;
        }
        // SAFETY: dup'ing a valid fd is sound; we own the dup and the
        // connection closes its own copy when `conn` drops below.
        let dup = unsafe { libc::dup(raw) };
        if dup < 0 {
            tracing::warn!(error = %std::io::Error::last_os_error(), "dup guest conn fd failed");
            continue;
        }
        // SAFETY: `dup` returned a fresh valid fd we now own.
        let owned_fd = unsafe { OwnedFd::from_raw_fd(dup) };
        delivery(owned_fd);
        // `conn` drops here — VZ's ObjC-level fd closes, but our dup keeps
        // the kernel-side connection open for the delivered stream.
        drop(conn);
    }
}

/// Schedule a closure on `queue` and await completion. Used for VZ
/// setters that take no completion handler (`setSocketListener:forPort:`,
/// `removeSocketListenerForPort:`).
async fn run_on_queue<F>(queue: &DispatchQueue, work: F)
where
    F: FnOnce() + Send + 'static,
{
    let (tx, rx) = oneshot::channel::<()>();
    let tx = Mutex::new(Some(tx));
    queue.exec_async(move || {
        work();
        if let Some(tx) = tx.lock().expect("oneshot mutex poisoned").take() {
            let _ = tx.send(());
        }
    });
    let _ = rx.await;
}

// ---- AsyncFd<RawFd> wrapper as AsyncRead+AsyncWrite ------------------

/// AsyncRead+AsyncWrite over a connected byte-stream socket fd (a VZ
/// vsock connection's fd). Readiness-polled via tokio's `AsyncFd`.
struct AsyncRawFdStream {
    inner: tokio::io::unix::AsyncFd<OwnedFd>,
}

impl AsyncRawFdStream {
    fn new(fd: OwnedFd) -> std::io::Result<Self> {
        let raw = fd.as_raw_fd();
        // SAFETY: fcntl on a valid fd is sound.
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFL, 0) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if flags & libc::O_NONBLOCK == 0 {
            // SAFETY: fcntl on a valid fd is sound.
            let r = unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) };
            if r < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(Self {
            inner: tokio::io::unix::AsyncFd::new(fd)?,
        })
    }
}

impl AsyncRead for AsyncRawFdStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        loop {
            let mut guard = match self.inner.poll_read_ready(cx) {
                std::task::Poll::Ready(Ok(g)) => g,
                std::task::Poll::Ready(Err(e)) => return std::task::Poll::Ready(Err(e)),
                std::task::Poll::Pending => return std::task::Poll::Pending,
            };
            let unfilled = buf.initialize_unfilled();
            // SAFETY: `read(2)` on a byte-stream fd is sound.
            let n = unsafe {
                libc::read(
                    self.inner.get_ref().as_raw_fd(),
                    unfilled.as_mut_ptr() as *mut _,
                    unfilled.len(),
                )
            };
            match n {
                n if n > 0 => {
                    buf.advance(n as usize);
                    return std::task::Poll::Ready(Ok(()));
                }
                0 => return std::task::Poll::Ready(Ok(())), // EOF
                _ => {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        guard.clear_ready();
                        continue;
                    }
                    return std::task::Poll::Ready(Err(err));
                }
            }
        }
    }
}

impl AsyncWrite for AsyncRawFdStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        loop {
            let mut guard = match self.inner.poll_write_ready(cx) {
                std::task::Poll::Ready(Ok(g)) => g,
                std::task::Poll::Ready(Err(e)) => return std::task::Poll::Ready(Err(e)),
                std::task::Poll::Pending => return std::task::Poll::Pending,
            };
            // SAFETY: `write(2)` on a byte-stream fd is sound.
            let n = unsafe {
                libc::write(
                    self.inner.get_ref().as_raw_fd(),
                    bytes.as_ptr() as *const _,
                    bytes.len(),
                )
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::WouldBlock {
                    guard.clear_ready();
                    continue;
                }
                return std::task::Poll::Ready(Err(err));
            }
            return std::task::Poll::Ready(Ok(n as usize));
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // SAFETY: shutdown(2) on a connected socket fd is sound. Half-close
        // the write side so the peer sees EOF; the read side stays open
        // until the fd drops.
        let _ = unsafe { libc::shutdown(self.inner.get_ref().as_raw_fd(), libc::SHUT_WR) };
        std::task::Poll::Ready(Ok(()))
    }
}

// ---- VZVirtioSocketListener delegate ---------------------------------

use listener_delegate::VsockListenerDelegate;

mod listener_delegate {
    use super::*;
    use objc2::define_class;
    use objc2::DefinedClass;
    use objc2_foundation::NSObject;
    use objc2_virtualization::VZVirtioSocketListenerDelegate;

    /// State held inside the delegate instance — the channel accepted
    /// connections are pushed through.
    pub(crate) struct DelegateIvars {
        pub(crate) tx:
            Mutex<Option<mpsc::UnboundedSender<Sendable<Retained<VZVirtioSocketConnection>>>>>,
    }

    use objc2::runtime::NSObjectProtocol;

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "EngramVZVsockListenerDelegate"]
        #[ivars = DelegateIvars]
        pub(crate) struct VsockListenerDelegate;

        // Every ObjC class must declare NSObjectProtocol; we don't
        // override anything (defaults inherited from NSObject).
        unsafe impl NSObjectProtocol for VsockListenerDelegate {}

        unsafe impl VZVirtioSocketListenerDelegate for VsockListenerDelegate {
            #[unsafe(method(listener:shouldAcceptNewConnection:fromSocketDevice:))]
            fn should_accept(
                &self,
                _listener: &VZVirtioSocketListener,
                connection: &VZVirtioSocketConnection,
                _device: &VZVirtioSocketDevice,
            ) -> bool {
                let ivars = self.ivars();
                let tx = ivars.tx.lock().expect("delegate tx mutex poisoned");
                if let Some(tx) = tx.as_ref() {
                    let conn = Sendable(super::retain_connection(connection));
                    // YES = accept: the consumer takes ownership of the fd.
                    tx.send(conn).is_ok()
                } else {
                    false
                }
            }
        }
    );

    impl VsockListenerDelegate {
        pub(crate) fn new(
            tx: mpsc::UnboundedSender<Sendable<Retained<VZVirtioSocketConnection>>>,
        ) -> Retained<Self> {
            let this = Self::alloc().set_ivars(DelegateIvars {
                tx: Mutex::new(Some(tx)),
            });
            // SAFETY: `init` returns a fully-initialised retained instance.
            unsafe { objc2::msg_send![super(this), init] }
        }
    }
}

/// Re-retain a borrowed `VZVirtioSocketConnection` (objc2 doesn't expose
/// a public `retain()` on the generated binding).
fn retain_connection(conn: &VZVirtioSocketConnection) -> Retained<VZVirtioSocketConnection> {
    // SAFETY: `conn` is a valid pointer; `retain` returns a +1 reference
    // per ObjC ARC convention.
    unsafe { Retained::retain(conn as *const _ as *mut _) }
        .expect("VZVirtioSocketConnection retain returned nil")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_uds_path_appends_underscore_port() {
        let base = std::path::PathBuf::from("/tmp/sb-abc.vsock");
        let p = port_uds_path(&base, 1024);
        assert_eq!(p.to_string_lossy(), "/tmp/sb-abc.vsock_1024");
    }

    #[test]
    fn bridge_error_io_round_trips_into_sandbox_error() {
        let err = BridgeError::Bind(
            std::path::PathBuf::from("/x"),
            std::io::Error::other("boom"),
        );
        let sb: engram_core::SandboxError = err.into();
        assert!(sb.to_string().contains("boom"));
        assert!(sb.to_string().contains("/x"));
    }

    #[test]
    fn no_socket_device_error_message_is_actionable() {
        let sb: engram_core::SandboxError = BridgeError::NoSocketDevice.into();
        assert!(sb.to_string().contains("VZVirtioSocketDevice"));
    }

    /// `connection_ptr_to_fd` surfaces the NSError branch as an `Err`
    /// without touching the (null) connection pointer.
    #[test]
    fn connection_ptr_to_fd_reports_nil_connection() {
        let r = connection_ptr_to_fd(std::ptr::null_mut(), std::ptr::null_mut());
        assert!(r.is_err(), "nil conn + nil err must be an error");
        assert!(r.unwrap_err().contains("nil connection"));
    }

    /// A `ConnDelivery` wraps the fd VZ would hand us into a stream that
    /// carries the guest's bytes. Drive it with a socketpair standing in
    /// for a `VZVirtioSocketConnection` fd (no live VM needed): one end is
    /// the "guest" writer, the other is the connection fd the delegate
    /// would deliver. This exercises `AsyncRawFdStream` + the sink handoff
    /// shape used by harness / upload delivery, minus the ObjC delegate
    /// (which requires a running VM).
    #[tokio::test]
    async fn conn_delivery_wraps_fd_into_readable_stream() {
        let mut fds = [0i32; 2];
        // SAFETY: socketpair(2) on a 2-int array is sound.
        let r = unsafe { libc::socketpair(libc::AF_LOCAL, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
        assert_eq!(r, 0, "socketpair");
        // SAFETY: socketpair just returned two valid fds.
        let conn_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let guest_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let delivery: ConnDelivery = Arc::new(move |fd: OwnedFd| {
            let tx = tx.clone();
            let mut stream = AsyncRawFdStream::new(fd).expect("wrap fd");
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16];
                let n = stream.read(&mut buf).await.expect("read");
                let _ = tx.send(buf[..n].to_vec());
            });
        });
        delivery(conn_fd);

        // The in-guest side writes; the delivered stream should read it.
        use std::os::fd::IntoRawFd;
        // SAFETY: `guest_fd` is a valid owned socket fd.
        let guest_std =
            unsafe { std::os::unix::net::UnixStream::from_raw_fd(guest_fd.into_raw_fd()) };
        guest_std.set_nonblocking(true).expect("nonblocking");
        let mut guest = tokio::net::UnixStream::from_std(guest_std).expect("tokio uds");
        guest.write_all(b"ping").await.expect("guest write");

        let got = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("delivery within 2s")
            .expect("delivered bytes");
        assert_eq!(&got, b"ping");
    }
}
