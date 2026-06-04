//! Bridge between Apple's VZ multi-port virtio-console device and
//! the UDS-shaped surface that `engram-sandbox-firecracker` exposes
//! to the rest of the codebase. Lets `start_agent` /
//! `exec_stream` / the harness sink keep their FC-shaped wire calls
//! unchanged while the underlying VZ device class swaps from
//! virtio-vsock to multi-port virtio-console.
//!
//! # Three ports, two directions
//!
//! ```text
//!   port 1024 (host → guest, agentd):    UDS at <base>_1024  ←→  /dev/hvc1
//!   port 1025 (host → guest, bootstrap): UDS at <base>_1025  ←→  /dev/hvc2
//!   port 1026 (guest → host, harness):              pipe pair  ←→  /dev/hvc3 → harness_sink
//! ```
//!
//! For each port we create a host-side pipe pair (one for each
//! direction) at VM-config time and hand the VZ-facing ends to a
//! `VZFileHandleSerialPortAttachment` on a
//! `VZVirtioConsolePortConfiguration`. The guest sees the matching
//! `/dev/hvcN` device. Because virtio-console is universally
//! compiled into Linux kernels, this works on any standard arm64
//! cloud-image kernel — no `CONFIG_VIRTIO_VSOCKETS=y` needed.
//!
//! For ports 1024/1025 we bind a `UnixListener` and pump bytes
//! between each accepted connection and the per-port pipe. Multiple
//! concurrent UDS accepts on the same port serialize via a
//! `tokio::sync::Mutex` since virtio-console gives one byte stream
//! per port — interleaving writes would corrupt frames.
//!
//! For port 1026 we feed the guest's bytes directly to a duplex
//! stream and hand the other half to `HarnessSink`. The vsock-era
//! `VZVirtioSocketListener` delegate class is gone — virtio-console
//! has no "accept" semantic, the guest just writes and the host
//! reads, and that pump is what we hand to the sink.
//!
//! # Cleanup
//!
//! `ConsoleBridge::stop` aborts every pump task and removes the
//! UDS files. `Drop` aborts tasks as a backstop. The host-side
//! pipe fds are owned by `OwnedFd`s on the bridge and dropped here;
//! the VZ-side fds are owned by `VZFileHandleSerialPortAttachment`'s
//! NSFileHandles and get released when the VM drops.

use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use engram_core::traits::sandbox::{HarnessByteStream, HarnessSink};
use objc2::rc::Retained;
use objc2::AnyThread;
use objc2_foundation::{NSFileHandle, NSString, NSUInteger};
use objc2_virtualization::{
    VZFileHandleSerialPortAttachment, VZSerialPortAttachment, VZVirtioConsoleDeviceConfiguration,
    VZVirtioConsolePortConfiguration,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;

/// Well-known port numbers shared with `engram-sandbox-firecracker`
/// and the in-guest binaries. Order here is the order ports are
/// added to the device config; the guest sees them as
/// `/dev/hvc1`, `/dev/hvc2`, `/dev/hvc3` (`/dev/hvc0` is the
/// kernel boot console attached to host stderr).
pub(crate) const PORT_AGENTD: u32 = 1024;
pub(crate) const PORT_BOOTSTRAP: u32 = 1025;
pub(crate) const PORT_HARNESS: u32 = 1026;

/// Order ports are configured. Guest's hvc index follows this
/// ordering: hvc1 = first entry (PORT_AGENTD), hvc2, hvc3.
/// `engram-transport`'s console impl uses the same mapping.
const PORTS: &[u32] = &[PORT_AGENTD, PORT_BOOTSTRAP, PORT_HARNESS];

/// Errors from the bridge layer. Mostly thin wrappers over
/// `std::io::Error` annotated with which port + path tripped.
#[derive(Debug)]
pub(crate) enum BridgeError {
    /// `pipe(2)` failed.
    Pipe(std::io::Error),
    /// `UnixListener::bind` failed for a host-initiated port.
    Bind(PathBuf, std::io::Error),
    /// Setting an fd non-blocking via `fcntl` failed.
    Fcntl(std::io::Error),
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pipe(e) => write!(f, "pipe(2) failed: {e}"),
            Self::Bind(p, e) => write!(f, "bind {} failed: {e}", p.display()),
            Self::Fcntl(e) => write!(f, "fcntl(F_SETFL O_NONBLOCK) failed: {e}"),
        }
    }
}

impl std::error::Error for BridgeError {}

impl From<BridgeError> for engram_core::SandboxError {
    fn from(e: BridgeError) -> Self {
        engram_core::SandboxError::Vm(Box::new(e))
    }
}

// ---- Device config + host-side fds ------------------------------------

/// Host-side fd-pair for one virtio-console port. The VZ-facing
/// counterparts are owned by NSFileHandles inside the
/// `VZFileHandleSerialPortAttachment` constructed alongside.
pub(crate) struct HostPortFds {
    /// Read end: bytes the guest wrote to `/dev/hvcN` arrive here.
    pub(crate) read_from_guest: OwnedFd,
    /// Write end: bytes written here become readable on the guest's
    /// `/dev/hvcN`.
    pub(crate) write_to_guest: OwnedFd,
}

/// Per-port host-side fd-pairs collected at VM-config time.
pub(crate) struct ConsolePortFds {
    pub(crate) by_port: BTreeMap<u32, HostPortFds>,
}

/// Build the `VZVirtioConsoleDeviceConfiguration` (with three
/// ports) plus the parallel host-side fd map that
/// `ConsoleBridge::start` consumes.
///
/// The host-side fds are returned in `OwnedFd` form so they're
/// closed if the caller drops the result without spinning up the
/// bridge.
pub(crate) fn build_console_device(
) -> Result<(Retained<VZVirtioConsoleDeviceConfiguration>, ConsolePortFds), BridgeError> {
    // SAFETY: every call below operates on freshly-allocated ObjC
    // objects whose lifetimes are managed via `Retained`. None of
    // them have started running on a queue yet, so there's no
    // concurrent access to worry about. NSFileHandle constructors
    // take ownership of the supplied fds (`closeOnDealloc: true`)
    // so the fd lifetimes ride with the NSFileHandle, which rides
    // with the VZ device retain.
    unsafe {
        let device = VZVirtioConsoleDeviceConfiguration::new();
        // VZ provides a pre-allocated (empty) port array on the
        // device; we populate it via setObject_atIndexedSubscript.
        // There's no setPorts setter — Apple's API keeps `ports`
        // read-only and expects in-place mutation of the array.
        let array = device.ports();
        array.setMaximumPortCount(PORTS.len() as u32);

        let mut by_port: BTreeMap<u32, HostPortFds> = BTreeMap::new();
        for (idx, &port) in PORTS.iter().enumerate() {
            // socketpair(AF_LOCAL, SOCK_STREAM) instead of pipe(2).
            // Initial bring-up used pipes here, but VZ's
            // VZFileHandleSerialPortAttachment doesn't drain the
            // guest's virtio-tx queue reliably when its read fd is
            // a unidirectional pipe — the guest's second write
            // blocks indefinitely after a successful first write.
            // Switching to bidirectional Unix-domain stream sockets
            // lets VZ's runloop signal the guest properly. We pass
            // one end of the pair as both VZ's "read" and "write"
            // file handles; the other end stays on the host. Since
            // they're bidirectional, that's all we need.
            let (vz_end, host_end) = make_socketpair()?;

            // VZ's NSFileHandles. closeOnDealloc=true gives the VZ
            // side ownership of its end's lifetime.
            let vz_fd_for_reading = vz_end.try_clone().map_err(BridgeError::Pipe)?;
            let vz_read_handle = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
                NSFileHandle::alloc(),
                vz_fd_for_reading.into_raw_fd(),
                true,
            );
            let vz_write_handle = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
                NSFileHandle::alloc(),
                vz_end.into_raw_fd(),
                true,
            );
            let attachment =
                VZFileHandleSerialPortAttachment::initWithFileHandleForReading_fileHandleForWriting(
                    VZFileHandleSerialPortAttachment::alloc(),
                    Some(&vz_read_handle),
                    Some(&vz_write_handle),
                );
            let attachment_super: Retained<VZSerialPortAttachment> =
                Retained::cast_unchecked(attachment);

            let port_cfg = VZVirtioConsolePortConfiguration::new();
            // isConsole=false → guest exposes as `/dev/vport*p*`
            // (a regular virtio-console data port). With true the
            // kernel registers them as HVC consoles, but on the
            // VZ multi-port path we hit ENXIO when opening the
            // resulting hvc node — VZ doesn't appear to mark
            // multi-port HVC consoles as "active" the way the
            // kernel needs. Data-port mode is more reliable and
            // the wire is the same.
            port_cfg.setIsConsole(false);
            // Symbolic name visible at /sys/class/virtio-ports/<dev>/name.
            // engram-transport's port_to_device looks up the matching
            // sysfs entry by this name, then opens /dev/<basename>.
            let name = NSString::from_str(&format!("engram-port-{port}"));
            port_cfg.setName(Some(&name));
            port_cfg.setAttachment(Some(&attachment_super));

            array.setObject_atIndexedSubscript(Some(&port_cfg), idx as NSUInteger);

            // Host-side: one fd, used for both reading guest data
            // and writing to the guest. We dup() it so the bridge's
            // pump tasks can each own one direction.
            let host_for_read = host_end.try_clone().map_err(BridgeError::Pipe)?;
            by_port.insert(
                port,
                HostPortFds {
                    read_from_guest: host_for_read,
                    write_to_guest: host_end,
                },
            );
        }

        Ok((device, ConsolePortFds { by_port }))
    }
}

/// `socketpair(AF_LOCAL, SOCK_STREAM)` wrapper. Returns the two
/// connected ends as `OwnedFd`s, both set non-blocking. We use
/// socketpair instead of pipe(2) because VZ's
/// VZFileHandleSerialPortAttachment drains pipe-backed read fds
/// unreliably (see comment in `build_console_device`); bidirectional
/// stream sockets let VZ's runloop ack guest writes correctly.
fn make_socketpair() -> Result<(OwnedFd, OwnedFd), BridgeError> {
    let mut fds = [0i32; 2];
    // SAFETY: socketpair(2) on a 2-int array is sound.
    let r = unsafe { libc::socketpair(libc::AF_LOCAL, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
    if r != 0 {
        return Err(BridgeError::Pipe(std::io::Error::last_os_error()));
    }
    // SAFETY: socketpair(2) just gave us valid fds.
    let a = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let b = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    set_nonblocking(a.as_raw_fd())?;
    set_nonblocking(b.as_raw_fd())?;
    Ok((a, b))
}

fn set_nonblocking(raw: std::os::fd::RawFd) -> Result<(), BridgeError> {
    // SAFETY: fcntl on a valid fd is sound.
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL, 0) };
    if flags < 0 {
        return Err(BridgeError::Fcntl(std::io::Error::last_os_error()));
    }
    let r = unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if r < 0 {
        return Err(BridgeError::Fcntl(std::io::Error::last_os_error()));
    }
    Ok(())
}

// ---- ConsoleBridge ----------------------------------------------------

/// A bound virtio-console bridge for one VM. Owns the spawned pump
/// tasks; aborts them on drop.
pub(crate) struct ConsoleBridge {
    base_path: PathBuf,
    /// Aborted by `stop` and `Drop`.
    tasks: Vec<JoinHandle<()>>,
}

impl ConsoleBridge {
    /// Start pump tasks for each port. The host-side fds in
    /// `port_fds` are consumed.
    pub async fn start(
        base_path: PathBuf,
        port_fds: ConsolePortFds,
        sandbox_id: engram_core::SandboxId,
        harness_sink: Option<HarnessSink>,
    ) -> Result<Self, BridgeError> {
        let mut tasks: Vec<JoinHandle<()>> = Vec::new();
        let mut by_port = port_fds.by_port;

        for port in [PORT_AGENTD, PORT_BOOTSTRAP] {
            if let Some(fds) = by_port.remove(&port) {
                let uds_path = port_uds_path(&base_path, port);
                let _ = tokio::fs::remove_file(&uds_path).await;
                let listener = UnixListener::bind(&uds_path)
                    .map_err(|e| BridgeError::Bind(uds_path.clone(), e))?;
                tracing::debug!(port, path = %uds_path.display(), "vz console host-initiated bound");
                tasks.push(tokio::spawn(host_initiated_pump(listener, fds, port)));
            }
        }

        if let Some(fds) = by_port.remove(&PORT_HARNESS) {
            if let Some(sink) = harness_sink {
                tasks.push(tokio::spawn(guest_initiated_pump(
                    fds,
                    sandbox_id,
                    Arc::new(sink),
                )));
            } else {
                tracing::warn!("no harness sink registered; dropping port {PORT_HARNESS} pipes");
                drop(fds);
            }
        }

        Ok(Self { base_path, tasks })
    }

    /// Tear down: abort pump tasks and remove host-side UDS files.
    pub async fn stop(&mut self) {
        for h in self.tasks.drain(..) {
            h.abort();
        }
        for port in [PORT_AGENTD, PORT_BOOTSTRAP] {
            let _ = tokio::fs::remove_file(port_uds_path(&self.base_path, port)).await;
        }
    }
}

impl Drop for ConsoleBridge {
    fn drop(&mut self) {
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

// ---- Pump tasks -------------------------------------------------------

/// Bridge UDS ↔ pipe pair for a host-initiated port.
///
/// The pipe pair is shared across all UDS accepts on this port —
/// virtio-console gives exactly one byte stream per port, so
/// interleaving multiple accepts' writes would corrupt frames.
/// Holding the read/write fds inside `AsyncMutex`es serializes
/// accepts so only one consumer talks to the pipe at a time;
/// subsequent accepts wait until the prior pump finishes.
async fn host_initiated_pump(listener: UnixListener, fds: HostPortFds, port: u32) {
    let HostPortFds {
        read_from_guest,
        write_to_guest,
    } = fds;
    let reader = match AsyncRawFdStream::new(read_from_guest) {
        Ok(r) => Arc::new(AsyncMutex::new(r)),
        Err(e) => {
            tracing::error!(error = %e, port, "console pump: read fd setup failed");
            return;
        }
    };
    let writer = match AsyncRawFdStream::new(write_to_guest) {
        Ok(w) => Arc::new(AsyncMutex::new(w)),
        Err(e) => {
            tracing::error!(error = %e, port, "console pump: write fd setup failed");
            return;
        }
    };

    loop {
        let (host_stream, _peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, port, "console accept failed; pump exiting");
                return;
            }
        };
        let reader = reader.clone();
        let writer = writer.clone();
        tokio::spawn(async move {
            let read_guard = reader.lock().await;
            let write_guard = writer.lock().await;
            if let Err(e) = pump_uds_pipe(host_stream, read_guard, write_guard).await {
                tracing::warn!(error = %e, port, "console pump ended with error");
            }
        });
    }
}

/// One full-duplex pump cycle: host UDS ↔ pipe (read fd from VZ /
/// write fd to VZ). Returns when either side EOFs.
async fn pump_uds_pipe(
    host_stream: UnixStream,
    mut pipe_reader: tokio::sync::MutexGuard<'_, AsyncRawFdStream>,
    mut pipe_writer: tokio::sync::MutexGuard<'_, AsyncRawFdStream>,
) -> std::io::Result<()> {
    let (mut h_r, mut h_w) = host_stream.into_split();
    let h_to_g = async {
        let _ = tokio::io::copy(&mut h_r, &mut *pipe_writer).await;
        let _ = pipe_writer.shutdown().await;
    };
    let g_to_h = async {
        let _ = tokio::io::copy(&mut *pipe_reader, &mut h_w).await;
        let _ = h_w.shutdown().await;
    };
    tokio::select! {
        _ = h_to_g => {}
        _ = g_to_h => {}
    }
    Ok(())
}

/// Pump for the guest-initiated harness port. There's no UDS — the
/// guest writes to `/dev/vport3p2`, the host reads from
/// `read_from_guest`, and we deliver bytes to the harness sink via
/// a duplex stream. Symmetric for the host→guest direction.
async fn guest_initiated_pump(
    fds: HostPortFds,
    sandbox_id: engram_core::SandboxId,
    sink: Arc<HarnessSink>,
) {
    let HostPortFds {
        read_from_guest,
        write_to_guest,
    } = fds;
    let reader = match AsyncRawFdStream::new(read_from_guest) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "console harness pump: read fd setup failed");
            return;
        }
    };
    let writer = match AsyncRawFdStream::new(write_to_guest) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(error = %e, "console harness pump: write fd setup failed");
            return;
        }
    };
    let (sink_half, vz_half) = tokio::io::duplex(64 * 1024);
    let stream: HarnessByteStream = Box::pin(sink_half);
    sink(sandbox_id, stream);

    let (mut v_r, mut v_w) = tokio::io::split(vz_half);
    let (mut p_r, mut p_w) = (reader, writer);
    tokio::select! {
        _ = tokio::io::copy(&mut v_r, &mut p_w) => {},
        _ = tokio::io::copy(&mut p_r, &mut v_w) => {},
    }
}

// ---- AsyncFd<RawFd> wrapper as AsyncRead+AsyncWrite -------------------

/// AsyncRead+AsyncWrite over a raw byte-stream fd (here, a pipe end).
/// Same pattern the FC vsock_bridge.rs used for VZ-side fds; lifted
/// here verbatim because pipe fds have identical semantics.
struct AsyncRawFdStream {
    inner: tokio::io::unix::AsyncFd<OwnedFd>,
}

impl AsyncRawFdStream {
    fn new(fd: OwnedFd) -> std::io::Result<Self> {
        let raw = fd.as_raw_fd();
        // Pipes from `make_pipe` are already non-blocking; double-
        // checking is cheap and keeps this constructor reusable for
        // fds from other sources.
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFL, 0) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if flags & libc::O_NONBLOCK == 0 {
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
        // Pipes don't have shutdown; closing happens on drop.
        std::task::Poll::Ready(Ok(()))
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

    #[test]
    fn make_socketpair_returns_two_nonblocking_fds() {
        let (a, b) = make_socketpair().expect("socketpair creation");
        for fd in [a.as_raw_fd(), b.as_raw_fd()] {
            // SAFETY: fd is owned by `a`/`b` which outlive this call.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
            assert!(flags >= 0);
            assert_ne!(
                flags & libc::O_NONBLOCK,
                0,
                "socket end should be non-blocking"
            );
        }
    }

    #[test]
    fn build_console_device_yields_three_ports_with_fds() {
        let (_device, fds) = build_console_device().expect("device + fds construct");
        assert_eq!(fds.by_port.len(), 3);
        for &port in &[PORT_AGENTD, PORT_BOOTSTRAP, PORT_HARNESS] {
            assert!(fds.by_port.contains_key(&port));
        }
    }
}
