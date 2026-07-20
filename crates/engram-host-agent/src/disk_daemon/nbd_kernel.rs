//! The production Linux [`NbdKernel`] (ADR 0098 Phase 2, Flow B — P7).
//!
//! [`HostNbdKernel`] is the prod impl of the portable
//! [`engram_host_core::NbdKernel`] seam: it wraps `disk_daemon::nbd_netlink`
//! (the genl `NBD_CMD_CONNECT`/`RECONFIGURE`/`DISCONNECT` round-trips) and the
//! `/sys/block/nbdN/backend` sysfs read. The blocking genl syscalls run on
//! `spawn_blocking` so the async workers are never parked. It is a stateless
//! ZST — the runtime's attach/reattach flows bind one and drive the kernel
//! through it, so the CONNECT/RECONFIGURE/backend-identifier touches funnel
//! through the trait (the host-internal simulator supplies its own
//! `SimNbd`).

#![cfg(target_os = "linux")]

use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use engram_host_core::{ConnectedDevice, NbdConnectRequest, NbdKernel, NbdReconfigureRequest};

use super::nbd_netlink::{self, NbdNetlinkParams};

/// The production Linux NBD kernel control plane.
pub struct HostNbdKernel;

#[async_trait]
impl NbdKernel for HostNbdKernel {
    async fn connect(&self, req: NbdConnectRequest<'_>) -> io::Result<()> {
        let index = nbd_netlink::device_index(req.device)?;
        let backend_id = req.backend_identifier.to_string();
        let sock_fd = req.serve_fd;
        let size_bytes = req.size_bytes;
        let block_size = req.block_size;
        let timeout_secs = req.timeout_secs;
        let dead_conn_timeout_secs = req.dead_conn_timeout_secs;
        tokio::task::spawn_blocking(move || {
            let params = NbdNetlinkParams {
                index,
                sock_fd,
                timeout_secs,
                dead_conn_timeout_secs,
                backend_identifier: &backend_id,
            };
            nbd_netlink::connect_device(&params, size_bytes, block_size)
        })
        .await
        .map_err(io::Error::other)?
    }

    async fn reconfigure(&self, req: NbdReconfigureRequest<'_>) -> io::Result<()> {
        let index = nbd_netlink::device_index(req.device)?;
        let backend_id = req.backend_identifier.to_string();
        let sock_fd = req.serve_fd;
        let timeout_secs = req.timeout_secs;
        let dead_conn_timeout_secs = req.dead_conn_timeout_secs;
        tokio::task::spawn_blocking(move || {
            let params = NbdNetlinkParams {
                index,
                sock_fd,
                timeout_secs,
                dead_conn_timeout_secs,
                backend_identifier: &backend_id,
            };
            nbd_netlink::reconfigure_device(&params)
        })
        .await
        .map_err(io::Error::other)?
    }

    async fn disconnect(&self, device: &Path) -> io::Result<()> {
        let index = nbd_netlink::device_index(device)?;
        tokio::task::spawn_blocking(move || nbd_netlink::disconnect_device(index))
            .await
            .map_err(io::Error::other)?
    }

    fn backend_identifier(&self, device: &Path) -> Option<String> {
        // The identifier the kernel recorded at CONNECT time —
        // `/sys/block/nbdN/backend`. `None` when the attr is missing/
        // unreadable (device never netlink-configured, or a pre-identifier
        // kernel).
        let name = device.file_name()?.to_str()?;
        let raw = std::fs::read_to_string(format!("/sys/block/{name}/backend")).ok()?;
        let id = raw.trim();
        (!id.is_empty()).then(|| id.to_string())
    }

    fn connected_devices(&self) -> Vec<ConnectedDevice> {
        // Layer 2 (ADR 0098 §Phase 3, Wave 7b, #784): enumerate KERNEL ground
        // truth — every `/dev/nbdN` with a populated `/sys/block/nbdN/pid`. A
        // device is CONNECTED iff its pid attr holds a parseable pid; a free /
        // never-configured device has an absent-or-empty pid and is skipped.
        // Sorted by ordinal so the classification barrier sees a deterministic
        // inventory (ADR 0098 D5).
        let mut connected: Vec<(u32, ConnectedDevice)> = Vec::new();
        let block = match std::fs::read_dir("/sys/block") {
            Ok(rd) => rd,
            // No /sys/block (non-Linux-ish env) → an empty inventory: the
            // barrier reconciles nothing and reaps nothing, the fail-safe.
            Err(_) => return Vec::new(),
        };
        for entry in block.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(ordinal) = name.strip_prefix("nbd").and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            let pid = match std::fs::read_to_string(format!("/sys/block/{name}/pid")) {
                Ok(s) => match s.trim().parse::<i32>() {
                    Ok(pid) => pid,
                    // Absent/empty/garbage pid ⇒ not connected.
                    Err(_) => continue,
                },
                Err(_) => continue,
            };
            let device = PathBuf::from(format!("/dev/{name}"));
            let backend_id = self.backend_identifier(&device);
            connected.push((
                ordinal,
                ConnectedDevice {
                    device,
                    owner_pid: pid,
                    backend_id,
                },
            ));
        }
        connected.sort_by_key(|(ordinal, _)| *ordinal);
        connected.into_iter().map(|(_, d)| d).collect()
    }
}
