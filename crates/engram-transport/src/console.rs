//! virtio-console-backed [`Transport`] implementation.
//!
//! Each "port" maps to one of three multi-port virtio-console
//! devices the host (Apple VZ) configures at VM-config time. Port
//! lookup is by name via `/sys/class/virtio-ports/`: the host
//! configures each port with `name = "engram-port-<port>"` (see
//! `engram-sandbox-vz/src/console_bridge.rs`); the guest scans
//! sysfs for the matching name and opens the corresponding
//! `/dev/<basename>` device. The exact device-node naming
//! (`/dev/hvc<N>` for `is_console=1` ports, `/dev/vport<bus>p<N>`
//! for data ports) varies with the kernel build, so we resolve it
//! at runtime instead of hardcoding.
//!
//! # One stream per port, but accept can re-open
//!
//! Unlike vsock, virtio-console gives exactly one logical byte
//! stream per port. But the host can close its fd and reopen it
//! later (e.g. on snapshot/restore, when VZ pairs fresh
//! NSFileHandles). Each host close → guest read returns EOF.
//!
//! [`Listener::accept`] therefore reopens `/dev/hvcN` on every
//! call: first accept blocks until the host has its end ready and
//! returns the fresh fd; subsequent accepts after a guest-side
//! close + reopen mirror the same shape. This matches vsock's
//! "fresh stream per accept" semantic on the consumer side.
//!
//! # Why `tokio::fs::File`
//!
//! Linux exposes virtio-console ports as character devices with
//! standard read(2) / write(2) semantics. `tokio::fs::File`'s
//! blocking-pool I/O is fine for our throughput (small frames, low
//! rate); we don't need the readiness polling required for sockets.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::{BoxedStream, Listener, Transport};

/// Resolve a well-known port number to its in-guest device path by
/// scanning `/sys/class/virtio-ports/` for a port whose `name` file
/// matches `engram-port-<port>`. The corresponding device node is
/// `/dev/<basename-of-sysfs-entry>`.
///
/// Sync std::fs is fine here — the lookup runs once per dial/listen
/// at startup and reads ~3 small files; we'd save no real time
/// using tokio::fs.
fn port_to_device(port: u32) -> io::Result<PathBuf> {
    let want_name = format!("engram-port-{port}");
    let entries = std::fs::read_dir("/sys/class/virtio-ports").map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("read /sys/class/virtio-ports (kernel missing CONFIG_VIRTIO_CONSOLE?): {e}"),
        )
    })?;
    for entry in entries.flatten() {
        let name_path = entry.path().join("name");
        let name = std::fs::read_to_string(&name_path)
            .ok()
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if name == want_name {
            let dev_basename = entry.file_name();
            return Ok(PathBuf::from("/dev").join(dev_basename));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "virtio-console transport: no port named {want_name:?} under \
             /sys/class/virtio-ports/ (host config drift?)"
        ),
    ))
}

/// virtio-console transport. Stateless — every dial/listen call
/// opens a fresh fd via the per-port-name sysfs lookup.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConsoleTransport;

impl ConsoleTransport {
    pub const fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Transport for ConsoleTransport {
    async fn dial(&self, port: u32) -> io::Result<BoxedStream> {
        // dial doesn't need an exclusivity permit — there's one
        // dialer per binary in our usage (the harness adapter
        // dials port 1026 once and keeps it for the session).
        let fd = open_port(port).await?;
        let stream = ConsoleStream::new(fd, None)?;
        Ok(Box::pin(stream))
    }

    async fn listen(&self, port: u32) -> io::Result<Box<dyn Listener>> {
        // Validate the port maps to a known device path now, so a
        // bad port number fails at bind time rather than on first
        // accept.
        let _ = port_to_device(port)?;
        Ok(Box::new(ConsoleListener {
            port,
            permit: Arc::new(AsyncMutex::new(())),
        }))
    }
}

/// Open the port's `/dev/<name>` device for read+write with
/// O_NONBLOCK. Returns the raw fd wrapped in an `OwnedFd`. We
/// deliberately bypass `tokio::fs::File` here — that wrapper
/// serialises every read/write through a state machine + the
/// blocking thread pool, and on virtio-console char devices it
/// stalled mid-stream during initial bring-up (the second write
/// blocked indefinitely after a successful first). Using
/// `AsyncFd` directly lets us read/write via real non-blocking
/// I/O, with the kernel's normal poll-readiness signalling.
async fn open_port(port: u32) -> io::Result<OwnedFd> {
    let path = port_to_device(port)?;
    let path_for_err = path.clone();
    let path_for_open = path.clone();
    tokio::task::spawn_blocking(move || open_blocking(&path_for_open))
        .await
        .map_err(|e| io::Error::other(format!("spawn_blocking join: {e}")))?
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "open {} (virtio-console port {port}): {e}",
                    path_for_err.display()
                ),
            )
        })
}

fn open_blocking(path: &Path) -> io::Result<OwnedFd> {
    let path_c =
        std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).map_err(io::Error::other)?;
    // SAFETY: open(2) with valid CString path; fd ownership is
    // captured by OwnedFd::from_raw_fd below.
    let raw = unsafe {
        libc::open(
            path_c.as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: open(2) just gave us a valid fd.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Listener with mutex-based exclusivity. virtio-console data
/// ports allow only one concurrent open; if accept yielded
/// streams without serializing, the second open would fail with
/// EBUSY. We hold an `OwnedMutexGuard` inside each emitted
/// stream — the next `accept` waits for the prior stream to drop
/// (releasing the guard) before opening.
struct ConsoleListener {
    port: u32,
    permit: Arc<AsyncMutex<()>>,
}

#[async_trait]
impl Listener for ConsoleListener {
    async fn accept(&mut self) -> io::Result<BoxedStream> {
        // Wait for any prior stream from this listener to drop.
        let guard = self.permit.clone().lock_owned().await;
        let fd = open_port(self.port).await?;
        let stream = ConsoleStream::new(fd, Some(guard))?;
        Ok(Box::pin(stream))
    }
}

/// AsyncRead+AsyncWrite over a non-blocking virtio-console fd.
/// Optionally carries an `OwnedMutexGuard` from the listener's
/// per-port permit so dropping the stream lets the next accept
/// proceed.
struct ConsoleStream {
    inner: AsyncFd<OwnedFd>,
    _permit: Option<OwnedMutexGuard<()>>,
}

impl ConsoleStream {
    fn new(fd: OwnedFd, permit: Option<OwnedMutexGuard<()>>) -> io::Result<Self> {
        Ok(Self {
            inner: AsyncFd::new(fd)?,
            _permit: permit,
        })
    }
}

impl AsyncRead for ConsoleStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = match self.inner.poll_read_ready(cx) {
                Poll::Ready(Ok(g)) => g,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            let unfilled = buf.initialize_unfilled();
            // SAFETY: read(2) on a byte-stream fd is sound.
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
                    return Poll::Ready(Ok(()));
                }
                0 => return Poll::Ready(Ok(())), // EOF
                _ => {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::WouldBlock {
                        guard.clear_ready();
                        continue;
                    }
                    return Poll::Ready(Err(err));
                }
            }
        }
    }
}

impl AsyncWrite for ConsoleStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = match self.inner.poll_write_ready(cx) {
                Poll::Ready(Ok(g)) => g,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            // SAFETY: write(2) on a byte-stream fd is sound.
            let n = unsafe {
                libc::write(
                    self.inner.get_ref().as_raw_fd(),
                    bytes.as_ptr() as *const _,
                    bytes.len(),
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::WouldBlock {
                    guard.clear_ready();
                    continue;
                }
                return Poll::Ready(Err(err));
            }
            return Poll::Ready(Ok(n as usize));
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // virtio-console char devices don't have a shutdown
        // primitive; closing happens on drop.
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_to_device_errors_when_sysfs_entry_missing() {
        // On a typical CI/host (no virtio-console attached),
        // /sys/class/virtio-ports doesn't exist at all → the
        // function should return a clean NotFound-flavoured error
        // rather than panicking.
        if std::path::Path::new("/sys/class/virtio-ports").exists() {
            return; // can't easily simulate "missing port" on a host that has them
        }
        let err = port_to_device(1024).unwrap_err();
        assert!(
            err.to_string().contains("/sys/class/virtio-ports")
                || err.to_string().contains("CONFIG_VIRTIO_CONSOLE"),
            "error should hint at sysfs / kernel config: {err}"
        );
    }
}
