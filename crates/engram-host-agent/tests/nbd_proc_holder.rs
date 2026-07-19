//! ADR 0098 §Phase 3 (R6, #784 layer 1 / #769 gap A) — the KERNEL ASSUMPTION
//! the live-holder guard leans on, pinned at the real `/dev/nbdN` plane.
//!
//! The R6 stale-binding guard refuses to DISCONNECT a dead-owner NBD device
//! while a live process still holds its node open — the surviving FC guest
//! reading its rootfs after the host-agent (netlink server) died. The guard's
//! proof-of-death probe is a `/proc/*/fd` readlink scan
//! (`device_has_live_holder`). This test proves the one kernel fact the design
//! rests on: **an open fd on a real, netlink-CONNECTed `/dev/nbdN` is
//! detectable via the proc scan even after the netlink SERVER is dead** — the
//! fd table is per-process and independent of NBD server liveness, so a
//! survivor guest's held fd is visible for the guard to protect.
//!
//! Sized to the property (AGENTS.md): CONNECT one small device, hold ONE fd,
//! abandon the serve (server dead), scan. No FC VM, no reads, no timing loops.
//!
//! Gating + run (mirrors `nbd_netlink_reconfigure.rs` — no FC needed):
//!
//! ```sh
//! sudo modprobe nbd nbds_max=4
//! sudo -E cargo test -p engram-host-agent --test nbd_proc_holder \
//!     -- --ignored --nocapture
//! ```
//!
//! Self-skips when `/dev/nbdN` is missing or unwritable.
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::Arc;

use engram_chunk_store::cache::{ChunkCache, ChunkCacheConfig};
use engram_chunk_store::{ChunkStore, ManifestKind};
use engram_core::traits::BlobStorage;
use engram_core::types::manifest::ManifestRef;
use engram_host_agent::disk_daemon::{
    attach_manifest, device_has_live_holder, NbdSandboxState, NbdSlotAllocator,
};
use engram_host_core::DeviceHolder;
use engram_storage_local::LocalBlobStorage;

/// Clear any stale binding from a prior aborted run (idempotent).
fn clear_stale_nbd_binding(nbd_path: &std::path::Path) {
    let Some(idx) = nbd_path
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix("nbd"))
        .and_then(|s| s.parse::<u32>().ok())
    else {
        return;
    };
    for _ in 0..20 {
        let _ = engram_host_agent::disk_daemon::nbd_netlink::disconnect_device(idx);
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn preflight() -> Option<PathBuf> {
    let nbd_path = PathBuf::from(
        std::env::var("ENGRAM_TEST_NBD_DEVICE").unwrap_or_else(|_| "/dev/nbd0".to_string()),
    );
    if !nbd_path.exists() {
        eprintln!(
            "SKIP: {} not present — run `sudo modprobe nbd nbds_max=4`",
            nbd_path.display()
        );
        return None;
    }
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&nbd_path)
    {
        Ok(_) => {
            clear_stale_nbd_binding(&nbd_path);
            Some(nbd_path)
        }
        Err(e) => {
            eprintln!(
                "SKIP: cannot open {} R/W: {e} — run as root (`sudo -E`)",
                nbd_path.display()
            );
            None
        }
    }
}

#[tokio::test]
#[ignore]
async fn proc_scan_detects_open_fd_on_nbd_device_with_dead_server() {
    std::env::set_var("ENGRAM_NBD_KERNEL_TIMEOUT_SECS", "5");
    std::env::set_var("ENGRAM_NBD_DEAD_CONN_TIMEOUT_SECS", "60");

    let nbd_path = match preflight() {
        Some(p) => p,
        None => return,
    };
    let work = tempfile::tempdir().expect("tempdir");

    // A minimal 64 KiB single-chunk image — we never read it; it exists only to
    // give the CONNECT a backend to bind.
    let image = work.path().join("disk.img");
    std::fs::write(&image, vec![0u8; 64 * 1024]).expect("write image");
    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(work.path().join("blob")));
    let store = Arc::new(ChunkStore::new(blob));
    let mut cache_cfg = ChunkCacheConfig::new(work.path().join("chunk-cache"));
    cache_cfg.budget_bytes = 16 * 1024 * 1024;
    let cache = ChunkCache::new(cache_cfg);
    let manifest = store
        .chunk_file(&image, ManifestKind::Disk, Some(64 * 1024))
        .await
        .expect("chunk image");
    let manifest_ref = ManifestRef::new();
    store
        .put_manifest(manifest_ref, &manifest)
        .await
        .expect("put manifest");

    // CONNECT + serve: a real netlink-bound /dev/nbdN with a live server.
    let pool = NbdSlotAllocator::from_paths(vec![nbd_path.clone()]).expect("pool");
    let state = attach_manifest(
        manifest_ref,
        cache,
        store,
        &pool,
        u64::MAX,
        /*fork=*/ false,
    )
    .await
    .expect("netlink CONNECT attach");
    let device = state.device_path().to_path_buf();

    // The "surviving FC guest": open ONE fd on the device node and hold it.
    // O_NONBLOCK so open never waits on the block device; we never read.
    use std::os::unix::fs::OpenOptionsExt;
    let guest_fd = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&device)
        .expect("open device node as the 'guest' holder");

    // Kill the netlink SERVER (abandon the serve loop) but keep the binding —
    // exactly the survivor state after a host-agent roll: dead server, live
    // guest fd. `abandon` leaves the kernel device configured (dead-conn
    // parked); `drop(slot)` returns the pool slot.
    let NbdSandboxState {
        scheduler: _,
        backend: _backend,
        handle,
        slot,
    } = state;
    handle.abandon();
    drop(slot);

    // The property: the proc scan finds the guest's held fd even though the
    // server is dead.
    assert_eq!(
        device_has_live_holder(&device),
        DeviceHolder::LiveHolder,
        "the guest's open fd on {} must be detected by the proc scan while the \
         netlink server is dead (the R6 kernel assumption)",
        device.display(),
    );

    // Release the guest's fd → the scan now reports NoHolder (a genuinely-gone
    // guest), so a DISCONNECT would be legal. This is the negative half that
    // proves the scan discriminates, not just always-true.
    drop(guest_fd);
    assert_eq!(
        device_has_live_holder(&device),
        DeviceHolder::NoHolder,
        "once the guest's fd is closed, the proc scan reports no live holder",
    );

    // Clean teardown so the device is free for the next run.
    clear_stale_nbd_binding(&device);
}
