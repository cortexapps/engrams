//! Bridge between Apple's VZ vsock API (programmatic) and the
//! UDS-shaped vsock surface that `engram-sandbox-firecracker` exposes
//! to the rest of the codebase. Lets `start_agent` /
//! `exec_stream` / the harness sink keep their FC-shaped wire calls
//! unchanged while the underlying transport switches to VZ.
//!
//! # Three ports, two directions
//!
//! ```text
//!   port 1024 (host → guest, agentd):  UDS at <base>_1024  ←→  VZ.connectToPort(1024)
//!   port 1025 (host → guest, bootstrap): UDS at <base>_1025 ←→  VZ.connectToPort(1025)
//!   port 1026 (guest → host, harness): VZVirtioSocketListener  ←→  duplex → harness_sink
//! ```
//!
//! For `1024` / `1025`, we bind a `UnixListener` and on each accept
//! ask VZ to dial the corresponding port inside the guest, then pump
//! bytes between the two sides.
//!
//! For `1026`, we register a `VZVirtioSocketListener` (a custom objc2
//! delegate class). When the guest dials, the delegate accepts and
//! forwards the connection to a tokio task that builds a duplex pair
//! and hands one half to the registered `HarnessSink`. The other half
//! pumps bytes against the VZ-side fd. This avoids the synthetic
//! "host UDS" hop FC needs — the harness sink already takes any
//! `AsyncRead + AsyncWrite` stream.
//!
//! # Cleanup
//!
//! `VsockBridge::stop` aborts every pump task, removes the VZ-side
//! socket listener registration (so a subsequent restore doesn't
//! collide), and cleans up the UDS files. `Drop` calls `stop`
//! synchronously as a backstop.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use block2::RcBlock;
use engram_core::traits::sandbox::{HarnessByteStream, HarnessSink};
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_foundation::NSError;
use objc2_virtualization::{
    VZSocketDevice, VZVirtioSocketConnection, VZVirtioSocketDevice, VZVirtioSocketListener,
    VZVirtualMachine,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::vm::Sendable;

/// Vsock ports — same well-known values as `engram-sandbox-firecracker`
/// since the in-guest agentd / bootstrap / harness binaries listen on
/// the same numbers regardless of which VMM is hosting the VM.
const VSOCK_PORT_AGENTD: u32 = 1024;
const VSOCK_PORT_BOOTSTRAP: u32 = 1025;
const VSOCK_PORT_HARNESS: u32 = 1026;

/// Errors from the bridge layer.
#[derive(Debug)]
pub(crate) enum BridgeError {
    /// The live VM doesn't expose a `VZVirtioSocketDevice` — should
    /// be impossible since we attach one in `vm::build_configuration`,
    /// but worth a clean error if Apple ever changes that contract.
    NoSocketDevice,
    /// Couldn't bind the host-side UDS for one of the host-initiated
    /// ports. Most often `EADDRINUSE` from a stale file.
    Bind(PathBuf, std::io::Error),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSocketDevice => write!(
                f,
                "vz vm has no VZVirtioSocketDevice — bridge cannot run"
            ),
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

/// A bound vsock bridge for one VM.
///
/// Owns the spawned pump tasks plus the VZ-side socket-device handle
/// (held only for the cleanup `removeSocketListenerForPort` call;
/// the VM keeps its own retain).
pub(crate) struct VsockBridge {
    base_path: PathBuf,
    /// Aborted by `stop` and `Drop`.
    tasks: Vec<JoinHandle<()>>,
    /// `Some` while the harness listener is registered. Holding the
    /// device retain ensures we can call `removeSocketListenerForPort`
    /// at cleanup time.
    harness_listener: Option<Sendable<Retained<VZVirtioSocketDevice>>>,
}

impl VsockBridge {
    /// Bind UDS listeners for ports 1024/1025 and register the
    /// guest-listener on port 1026 (only if `harness_sink` is set).
    /// The VM must already be `start()`ed — VZ socket-device APIs
    /// only operate on a running machine.
    pub async fn start(
        vm: Sendable<Retained<VZVirtualMachine>>,
        queue: dispatch2::DispatchRetained<dispatch2::DispatchQueue>,
        base_path: PathBuf,
        harness_sink: Option<HarnessSink>,
    ) -> Result<Self, BridgeError> {
        // Dispatch the socket-device lookup onto the VM's queue so
        // we're definitely on a VZ-safe thread when we reach into
        // its `socketDevices` array.
        let socket_device = lookup_socket_device(&queue, &vm).await
            .ok_or(BridgeError::NoSocketDevice)?;
        let device = Sendable(socket_device);

        let mut tasks: Vec<JoinHandle<()>> = Vec::with_capacity(3);

        // Bind the host-initiated ports first so dialers (start_agent,
        // exec_stream) don't race a listener-not-yet-bound window
        // between create() returning and the bridge being live.
        for port in [VSOCK_PORT_AGENTD, VSOCK_PORT_BOOTSTRAP] {
            let uds_path = port_uds_path(&base_path, port);
            let _ = tokio::fs::remove_file(&uds_path).await; // tolerate stale
            let listener = UnixListener::bind(&uds_path)
                .map_err(|e| BridgeError::Bind(uds_path.clone(), e))?;
            tracing::debug!(port, path = %uds_path.display(), "vz vsock host-initiated bound");
            let device_for_task = device.clone_inner();
            let queue_for_task = queue.clone();
            tasks.push(tokio::spawn(host_initiated_pump(
                listener,
                device_for_task,
                queue_for_task,
                port,
            )));
        }

        // Register the guest-initiated listener if the caller passed
        // a harness sink. Without a sink there's no consumer, so
        // we'd just be discarding incoming connections.
        let harness_listener = if let Some(sink) = harness_sink {
            let sink = Arc::new(sink);
            let (conn_tx, conn_rx) =
                mpsc::unbounded_channel::<Sendable<Retained<VZVirtioSocketConnection>>>();
            let delegate = make_listener_delegate(conn_tx);
            // SAFETY: VZVirtioSocketListener::new returns a freshly
            // retained, non-nil instance per Apple's contract. We
            // attach our delegate immediately before handing it to
            // the device.
            let listener = unsafe {
                let listener = VZVirtioSocketListener::new();
                listener.setDelegate(Some(&objc2::runtime::ProtocolObject::from_retained(
                    delegate,
                )));
                listener
            };
            // setSocketListener_forPort must be called on the VM's
            // queue per Apple's threading contract.
            run_on_queue(&queue, {
                let device = device.clone_inner();
                let listener = Sendable(listener);
                move || unsafe {
                    device.setSocketListener_forPort(&listener, VSOCK_PORT_HARNESS);
                }
            })
            .await;

            // Spawn the consumer that turns each incoming
            // VZVirtioSocketConnection into a duplex stream and
            // hands it to harness_sink.
            let queue_for_task = queue.clone();
            tasks.push(tokio::spawn(harness_consumer(conn_rx, sink, queue_for_task)));
            Some(device.clone_inner())
        } else {
            None
        };

        Ok(Self {
            base_path,
            tasks,
            harness_listener,
        })
    }

    /// Tear down the bridge. Aborts all pump tasks, unregisters
    /// the guest-initiated listener, and removes the host-initiated
    /// UDS files.
    pub async fn stop(&mut self, queue: &dispatch2::DispatchQueue) {
        for h in self.tasks.drain(..) {
            h.abort();
        }
        if let Some(device) = self.harness_listener.take() {
            run_on_queue(queue, move || unsafe {
                device.removeSocketListenerForPort(VSOCK_PORT_HARNESS);
            })
            .await;
        }
        for port in [VSOCK_PORT_AGENTD, VSOCK_PORT_BOOTSTRAP] {
            let _ = tokio::fs::remove_file(port_uds_path(&self.base_path, port)).await;
        }
    }
}

impl Drop for VsockBridge {
    fn drop(&mut self) {
        // Best-effort task abort — async cleanup is the
        // responsibility of `stop`. If the caller forgot to call it
        // (only possible on panic paths), at least the pump tasks
        // get cancelled and the UDS files get cleaned by the next
        // bind attempt.
        for h in self.tasks.drain(..) {
            h.abort();
        }
    }
}

impl Sendable<Retained<VZVirtioSocketDevice>> {
    fn clone_inner(&self) -> Sendable<Retained<VZVirtioSocketDevice>> {
        Sendable(self.0.clone())
    }
}

pub(crate) fn port_uds_path(base: &Path, port: u32) -> PathBuf {
    let mut p = base.to_path_buf();
    let mut name = p.file_name().unwrap_or_default().to_owned();
    name.push(format!("_{port}"));
    p.set_file_name(name);
    p
}

/// Find and return the first `VZVirtioSocketDevice` attached to a
/// running VM, executed on the VM's queue. We attach exactly one
/// in `vm::build_configuration`, so [0] is always ours.
async fn lookup_socket_device(
    queue: &dispatch2::DispatchQueue,
    vm: &Sendable<Retained<VZVirtualMachine>>,
) -> Option<Retained<VZVirtioSocketDevice>> {
    let (tx, rx) =
        oneshot::channel::<Option<Sendable<Retained<VZVirtioSocketDevice>>>>();
    let vm = vm.clone_inner();
    let tx = Mutex::new(Some(tx));
    queue.exec_async(move || {
        // SAFETY: dispatched onto the VM's queue, so accessing the
        // socketDevices array is sound. The retained NSArray and
        // its elements live for the duration of this closure.
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
    let res = rx.await.ok()??;
    Some(res.0)
}

impl Sendable<Retained<VZVirtualMachine>> {
    fn clone_inner(&self) -> Sendable<Retained<VZVirtualMachine>> {
        Sendable(self.0.clone())
    }
}

/// Per-port pump: accept on the host UDS listener, dial the
/// corresponding port via VZ, and pipe bytes both ways. One
/// connection at a time per port — `start_agent` and `exec_stream`
/// are serialized on the host side anyway.
async fn host_initiated_pump(
    listener: UnixListener,
    device: Sendable<Retained<VZVirtioSocketDevice>>,
    queue: dispatch2::DispatchRetained<dispatch2::DispatchQueue>,
    port: u32,
) {
    loop {
        let (host_stream, _peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, port, "vz vsock accept failed; pump exiting");
                return;
            }
        };
        let device = device.clone_inner();
        let queue = queue.clone();
        // Each accepted connection gets its own task so a slow
        // consumer doesn't block the listener.
        tokio::spawn(async move {
            match dial_vz(&queue, &device, port).await {
                Ok(vz_fd) => {
                    if let Err(e) = pump(vz_fd, host_stream).await {
                        tracing::warn!(error = %e, port, "vz vsock pump ended with error");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, port, "vz vsock connectToPort failed");
                }
            }
        });
    }
}

/// Dial a guest port via VZ. Bridges the completion-handler
/// callback to a tokio oneshot. Returns the connection's
/// underlying fd as an `OwnedFd` — VZ retains the connection
/// until we close the fd.
async fn dial_vz(
    queue: &dispatch2::DispatchQueue,
    device: &Sendable<Retained<VZVirtioSocketDevice>>,
    port: u32,
) -> Result<OwnedFd, String> {
    let (tx, rx) = oneshot::channel::<Result<OwnedFd, String>>();
    let device = device.clone_inner();
    let tx = Mutex::new(Some(tx));
    let block = RcBlock::new(
        move |conn: *mut VZVirtioSocketConnection, err: *mut NSError| {
            let result: Result<OwnedFd, String> = if !err.is_null() {
                // SAFETY: VZ guarantees `err` is a valid retained
                // NSError when non-null.
                let err = unsafe { Retained::retain(err) };
                Err(err
                    .map(|e| e.localizedDescription().to_string())
                    .unwrap_or_else(|| "<nil error>".into()))
            } else if conn.is_null() {
                Err("VZ returned nil connection with nil error".into())
            } else {
                // SAFETY: VZ hands us a +0 (retained-on-our-behalf)
                // VZVirtioSocketConnection. We re-retain to own it
                // for the duration of the pump.
                let conn = unsafe { Retained::retain(conn) };
                match conn {
                    Some(conn) => {
                        // SAFETY: the VZ connection retains the
                        // socket open until `close` or release;
                        // `fileDescriptor` returns an fd we can
                        // duplicate via dup() to obtain ownership
                        // independent of the connection lifetime.
                        let raw = unsafe { conn.fileDescriptor() };
                        if raw < 0 {
                            Err("VZVirtioSocketConnection.fileDescriptor returned -1".into())
                        } else {
                            // SAFETY: dup'ing a valid fd is sound.
                            // We then own the dup'd fd (closed via
                            // OwnedFd's Drop). The connection's
                            // own copy is closed when `conn` drops.
                            let dup = unsafe { libc::dup(raw) };
                            if dup < 0 {
                                Err(format!(
                                    "dup VZVirtioSocketConnection fd failed: {}",
                                    std::io::Error::last_os_error()
                                ))
                            } else {
                                Ok(unsafe { OwnedFd::from_raw_fd(dup) })
                            }
                        }
                    }
                    None => Err("VZVirtioSocketConnection retain returned None".into()),
                }
            };
            if let Some(tx) = tx.lock().expect("oneshot mutex poisoned").take() {
                let _ = tx.send(result);
            }
        },
    );
    let block = Sendable(block);
    queue.exec_async(move || unsafe {
        device.connectToPort_completionHandler(port, &block);
    });
    rx.await
        .map_err(|_| "vz dial: completion channel dropped".to_string())?
}

/// Bidirectional copy between a vsock fd (VZ side) and a UnixStream
/// (host side). Returns when either side EOFs or errors.
async fn pump(vz_fd: OwnedFd, host_stream: UnixStream) -> std::io::Result<()> {
    // Wrap the fd via tokio's AsyncFd and a small AsyncRead/Write
    // shim. The fd is a regular byte-stream socket; we just need
    // readiness-polling to feed it into tokio's IO.
    let vz = AsyncRawFdStream::new(vz_fd)?;
    let (mut h_r, mut h_w) = host_stream.into_split();
    let (mut v_r, mut v_w) = tokio::io::split(vz);

    let h_to_v = async move {
        let _ = tokio::io::copy(&mut h_r, &mut v_w).await;
        let _ = v_w.shutdown().await;
    };
    let v_to_h = async move {
        let _ = tokio::io::copy(&mut v_r, &mut h_w).await;
        let _ = h_w.shutdown().await;
    };
    // Either direction completing tears down the other.
    tokio::select! {
        _ = h_to_v => {}
        _ = v_to_h => {}
    }
    Ok(())
}

/// AsyncFd wrapper that exposes a raw socket fd as
/// `AsyncRead + AsyncWrite`. VZ vsock connections expose a regular
/// byte-stream fd we read/write with `read(2)` / `write(2)`; this
/// keeps the pump portable to whatever the underlying socket family
/// happens to be (vsock on Linux, a Mach-port-backed socket on
/// macOS — either way, byte-stream semantics).
struct AsyncRawFdStream {
    inner: tokio::io::unix::AsyncFd<OwnedFd>,
}

impl AsyncRawFdStream {
    fn new(fd: OwnedFd) -> std::io::Result<Self> {
        // Set non-blocking so AsyncFd's readiness-polling works.
        let raw = fd.as_raw_fd();
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFL, 0) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let r = unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if r < 0 {
            return Err(std::io::Error::last_os_error());
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
        // SAFETY: shutdown(2) on a connected socket fd is sound.
        let _ = unsafe { libc::shutdown(self.inner.get_ref().as_raw_fd(), libc::SHUT_WR) };
        std::task::Poll::Ready(Ok(()))
    }
}

/// Consumer side of the harness listener: each `VZVirtioSocketConnection`
/// the delegate accepts becomes a duplex stream paired against a pump
/// task. The other half goes to the `HarnessSink`.
async fn harness_consumer(
    mut conn_rx: mpsc::UnboundedReceiver<Sendable<Retained<VZVirtioSocketConnection>>>,
    sink: Arc<HarnessSink>,
    _queue: dispatch2::DispatchRetained<dispatch2::DispatchQueue>,
) {
    while let Some(conn) = conn_rx.recv().await {
        // Extract the fd before the connection drops. `dup` it so
        // the pump task owns its own copy independent of the
        // connection's lifetime.
        let raw = unsafe { conn.fileDescriptor() };
        if raw < 0 {
            tracing::warn!("vz harness conn returned -1 fd; dropping");
            continue;
        }
        let dup = unsafe { libc::dup(raw) };
        if dup < 0 {
            tracing::warn!(error = %std::io::Error::last_os_error(), "dup harness fd failed");
            continue;
        }
        let owned_fd = unsafe { OwnedFd::from_raw_fd(dup) };
        // Build a duplex pair. One half is the AsyncRead+Write
        // we hand to the sink (looks like any other harness
        // stream); the other half pumps against the VZ fd.
        let (sink_half, vz_half) = tokio::io::duplex(64 * 1024);
        let stream: HarnessByteStream = Box::pin(sink_half);
        sink(stream);
        tokio::spawn(async move {
            let vz = match AsyncRawFdStream::new(owned_fd) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "AsyncRawFdStream::new failed for harness fd");
                    return;
                }
            };
            let (mut v_r, mut v_w) = tokio::io::split(vz);
            let (mut d_r, mut d_w) = tokio::io::split(vz_half);
            tokio::select! {
                _ = tokio::io::copy(&mut v_r, &mut d_w) => {},
                _ = tokio::io::copy(&mut d_r, &mut v_w) => {},
            }
            // dup'd fd drops here.
        });
        // `conn` drops here — VZ sees the original fd close at the
        // ObjC level, but our dup keeps the kernel-side connection
        // open while the pump runs.
        drop(conn);
    }
}

/// Schedule a closure on `queue` and await completion. Used for
/// VZ ops that take no completion handler (sync setters like
/// `setSocketListener_forPort`).
async fn run_on_queue<F>(queue: &dispatch2::DispatchQueue, work: F)
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

// ---- VZVirtioSocketListener delegate ----------------------------------

/// Custom objc2 class that fulfils `VZVirtioSocketListenerDelegate`.
/// The delegate is held alive by the `VZVirtioSocketListener` it's
/// attached to; we hand each accepted connection back through an
/// `mpsc::UnboundedSender`.
fn make_listener_delegate(
    tx: mpsc::UnboundedSender<Sendable<Retained<VZVirtioSocketConnection>>>,
) -> Retained<VsockListenerDelegate> {
    let delegate = VsockListenerDelegate::new(tx);
    delegate
}

mod listener_delegate {
    use super::*;
    use objc2::define_class;
    use objc2::DefinedClass;
    use objc2_foundation::NSObject;
    use objc2_virtualization::VZVirtioSocketListenerDelegate;

    /// State held inside the delegate instance — the channel we
    /// push accepted connections through.
    pub(crate) struct DelegateIvars {
        pub(crate) tx:
            Mutex<Option<mpsc::UnboundedSender<Sendable<Retained<VZVirtioSocketConnection>>>>>,
    }

    use objc2::runtime::{Bool, NSObjectProtocol};

    define_class!(
        #[unsafe(super(NSObject))]
        #[name = "EngramVZVsockListenerDelegate"]
        #[ivars = DelegateIvars]
        pub(crate) struct VsockListenerDelegate;

        // Every ObjC class needs to declare NSObjectProtocol; the
        // protocol-implementor side of `extern_protocol!` requires
        // it as a supertrait. Defaults are inherited from NSObject
        // — we don't override anything.
        unsafe impl NSObjectProtocol for VsockListenerDelegate {}

        unsafe impl VZVirtioSocketListenerDelegate for VsockListenerDelegate {
            #[unsafe(method(listener:shouldAcceptNewConnection:fromSocketDevice:))]
            fn should_accept(
                &self,
                _listener: &VZVirtioSocketListener,
                connection: &VZVirtioSocketConnection,
                _device: &VZVirtioSocketDevice,
            ) -> Bool {
                let ivars = self.ivars();
                let tx = ivars.tx.lock().expect("delegate tx mutex poisoned");
                if let Some(tx) = tx.as_ref() {
                    let conn = Sendable(super::ConnectionRetainExt::retain(connection));
                    if tx.send(conn).is_err() {
                        return Bool::NO;
                    }
                    Bool::YES
                } else {
                    Bool::NO
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
            // SAFETY: `init` returns a fully-initialized retained
            // instance per ObjC's contract.
            unsafe { objc2::msg_send![super(this), init] }
        }
    }
}

use listener_delegate::VsockListenerDelegate;

// objc2's `connection.retain()` isn't a public method on the
// generated bindings, but `Retained::from(retain via msg_send)` is.
// We add a small helper trait so the delegate's hot path stays
// readable.
trait ConnectionRetainExt {
    fn retain(&self) -> Retained<VZVirtioSocketConnection>;
}

impl ConnectionRetainExt for VZVirtioSocketConnection {
    fn retain(&self) -> Retained<VZVirtioSocketConnection> {
        // SAFETY: `self` is a valid pointer; sending `retain`
        // returns a +1 retained reference per ObjC ARC convention.
        unsafe { Retained::retain(self as *const _ as *mut _) }
            .expect("VZVirtioSocketConnection retain returned nil")
    }
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
}
