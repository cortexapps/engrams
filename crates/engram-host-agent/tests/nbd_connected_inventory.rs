//! ADR 0098 §Phase 3 (Wave 7b, #784 layer 2) — the SYSFS-INVENTORY ASSUMPTION
//! the kernel-derived rehydrate inventory leans on, pinned at the real
//! `/dev/nbdN` plane.
//!
//! Layer 2 replaces the record-derived rehydrate work-list with KERNEL GROUND
//! TRUTH: `HostNbdKernel::connected_devices()` enumerates every `/dev/nbdN` the
//! kernel currently has CONNECTED by scanning `/sys/block/nbd*/pid`. The whole
//! classification barrier reconciles records AGAINST this list, so a survivor
//! invisible to records is still SEEN (and quarantined) rather than skipped.
//! This test proves the one kernel fact that rests on: **a netlink-CONNECTed
//! `/dev/nbdN` shows up in the sysfs scan with a populated pid and its recorded
//! backend id, and a device with no live binding does NOT** — so the inventory
//! is neither blind to a live survivor nor haunted by a free slot.
//!
//! Sized to the property (AGENTS.md): CONNECT one small device, enumerate,
//! DISCONNECT, enumerate. No FC VM, no reads, no timing loops.
//!
//! Gating + run (mirrors `nbd_proc_holder.rs` — no FC needed):
//!
//! ```sh
//! sudo modprobe nbd nbds_max=4
//! sudo -E cargo test -p engram-host-agent --test nbd_connected_inventory \
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
    attach_manifest, HostNbdKernel, NbdSandboxState, NbdSlotAllocator,
};
use engram_host_core::NbdKernel;
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

fn pid_file(device: &std::path::Path) -> Option<i32> {
    let name = device.file_name()?.to_str()?;
    std::fs::read_to_string(format!("/sys/block/{name}/pid"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[tokio::test]
#[ignore]
async fn connected_devices_reflects_kernel_ground_truth() {
    std::env::set_var("ENGRAM_NBD_KERNEL_TIMEOUT_SECS", "5");
    std::env::set_var("ENGRAM_NBD_DEAD_CONN_TIMEOUT_SECS", "60");

    let nbd_path = match preflight() {
        Some(p) => p,
        None => return,
    };
    let kernel = HostNbdKernel;

    // BEFORE: a freshly-cleared device carries no live binding, so the
    // kernel-derived inventory must not list it (no phantom free slot).
    assert!(
        !kernel
            .connected_devices()
            .iter()
            .any(|d| d.device == nbd_path),
        "{} appears CONNECTED before any CONNECT — a stale binding leaked into \
         the inventory",
        nbd_path.display(),
    );

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

    // AFTER CONNECT: the device is in the inventory, with the pid + backend id
    // the kernel recorded in sysfs (the whole assumption Layer 2 rests on).
    let inventory = kernel.connected_devices();
    let found = inventory
        .iter()
        .find(|d| d.device == device)
        .unwrap_or_else(|| {
            panic!(
                "CONNECTed device {} absent from connected_devices(): {inventory:?}",
                device.display()
            )
        });
    let sysfs_pid = pid_file(&device).expect("a CONNECTed device has a populated /sys pid");
    assert_eq!(
        found.owner_pid, sysfs_pid,
        "inventory owner_pid must match /sys/block/nbdN/pid ({sysfs_pid})",
    );
    assert!(
        found.backend_id.is_some(),
        "a CONNECTed device carries a recorded /sys/block/nbdN/backend id",
    );

    // Tear the serve + binding down.
    let NbdSandboxState {
        scheduler: _,
        backend: _backend,
        handle,
        slot,
    } = state;
    handle.abandon();
    drop(slot);
    clear_stale_nbd_binding(&device);

    // AFTER DISCONNECT: the pid clears and the device leaves the inventory.
    // Bounded poll — the netlink DISCONNECT + kernel teardown settles quickly.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let still_listed = kernel
            .connected_devices()
            .iter()
            .any(|d| d.device == device);
        if !still_listed {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{} still CONNECTED 10s after DISCONNECT — the inventory would keep \
             re-quarantining a genuinely-gone device",
            device.display(),
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}
