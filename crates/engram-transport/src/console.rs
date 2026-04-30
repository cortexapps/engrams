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
//! "fresh stream per accept" semantic on the consumer side
//! (`engram-bootstrap`'s outer loop, etc.).
//!
//! # Why `tokio::fs::File`
//!
//! Linux exposes virtio-console ports as character devices with
//! standard read(2) / write(2) semantics. `tokio::fs::File`'s
//! blocking-pool I/O is fine for our throughput (small frames, low
//! rate); we don't need the readiness polling required for sockets.

use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use tokio::fs::{File, OpenOptions};
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
        let file = open_port(port).await?;
        Ok(Box::pin(file))
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

/// Open the port's `/dev/<name>` device for read+write.
async fn open_port(port: u32) -> io::Result<File> {
    let path = port_to_device(port)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .await
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("open {} (virtio-console port {port}): {e}", path.display()),
            )
        })
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
        let file = open_port(self.port).await?;
        Ok(Box::pin(GuardedStream {
            file,
            _permit: guard,
        }))
    }
}

/// Wraps a `tokio::fs::File` with an `OwnedMutexGuard` so dropping
/// the stream releases the listener's per-port permit.
struct GuardedStream {
    file: File,
    _permit: OwnedMutexGuard<()>,
}

impl AsyncRead for GuardedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.file).poll_read(cx, buf)
    }
}

impl AsyncWrite for GuardedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.file).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.file).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.file).poll_shutdown(cx)
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
