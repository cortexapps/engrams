//! virtio-console-backed [`Transport`] implementation.
//!
//! Each "port" maps to one of three multi-port virtio-console
//! devices the host (Apple VZ) configures at VM-config time:
//!
//! ```text
//!   port 1024 (engram-agentd)        → /dev/hvc1
//!   port 1025 (engram-bootstrap)     → /dev/hvc2
//!   port 1026 (engram-harness-*)     → /dev/hvc3
//! ```
//!
//! `/dev/hvc0` is the kernel boot console; we leave it untouched.
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

use async_trait::async_trait;
use tokio::fs::OpenOptions;

use crate::{BoxedStream, Listener, Transport};

/// Map well-known port number to in-VM virtio-console device path.
/// The host side of the bake (engram-sandbox-vz's vm.rs) configures
/// the same three ports in the same order.
fn port_to_device(port: u32) -> io::Result<PathBuf> {
    let n = match port {
        1024 => 1, // engram-agentd
        1025 => 2, // engram-bootstrap
        1026 => 3, // engram-harness-*
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "virtio-console transport: port {other} is not mapped to a /dev/hvcN device"
                ),
            ));
        }
    };
    Ok(PathBuf::from(format!("/dev/hvc{n}")))
}

/// virtio-console transport. Stateless — every dial/listen call
/// opens a fresh fd to `/dev/hvcN`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConsoleTransport;

impl ConsoleTransport {
    pub const fn new() -> Self {
        Self
    }

    /// Open `/dev/hvc<N>` for read+write. Used by both `dial` and
    /// `listen` since the device is symmetric.
    async fn open(&self, port: u32) -> io::Result<BoxedStream> {
        let path = port_to_device(port)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .await
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("open {} (virtio-console port {port}): {e}", path.display()),
                )
            })?;
        Ok(Box::pin(file))
    }
}

#[async_trait]
impl Transport for ConsoleTransport {
    async fn dial(&self, port: u32) -> io::Result<BoxedStream> {
        self.open(port).await
    }

    async fn listen(&self, port: u32) -> io::Result<Box<dyn Listener>> {
        // Validate the port maps to a known device path now, so a
        // bad port number fails at bind time rather than on first
        // accept. We don't actually open the file until accept().
        let _ = port_to_device(port)?;
        Ok(Box::new(ConsoleListener { port }))
    }
}

/// Reopens `/dev/hvcN` on every [`accept`](Listener::accept). The
/// fresh open blocks until the host has its end of the
/// virtio-console port ready; subsequent reopens (after a host
/// close → guest EOF → guest close cycle) reattach the same way.
struct ConsoleListener {
    port: u32,
}

#[async_trait]
impl Listener for ConsoleListener {
    async fn accept(&mut self) -> io::Result<BoxedStream> {
        let path = port_to_device(self.port)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .await
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!(
                        "open {} (virtio-console port {}): {e}",
                        path.display(),
                        self.port
                    ),
                )
            })?;
        Ok(Box::pin(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_to_device_maps_known_ports_and_rejects_others() {
        assert_eq!(port_to_device(1024).unwrap(), PathBuf::from("/dev/hvc1"));
        assert_eq!(port_to_device(1025).unwrap(), PathBuf::from("/dev/hvc2"));
        assert_eq!(port_to_device(1026).unwrap(), PathBuf::from("/dev/hvc3"));
        let err = port_to_device(9999).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("9999"));
    }
}
