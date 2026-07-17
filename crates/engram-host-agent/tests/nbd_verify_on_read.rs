//! ADR 0098 P7 (Flow B) — the verify-on-read rider at the DEVICE plane.
//!
//! The 2026-07-16 session-85e0298a class was a survivor reading the
//! rolled-back base instead of its acked bytes after a pod roll. Flow B's
//! rehydrate seeds the predecessor's shutdown-spool dirty chunks into the
//! fresh backend BEFORE the RECONFIGURE (the kernel releases parked guest I/O
//! the instant it adopts our socket), and `reattach_manifest` now runs a
//! verify-on-read probe through the backend. This test closes the gap that
//! in-process probe cannot: it proves the property at the KERNEL DEVICE plane
//! — an O_DIRECT read of the seeded chunk returns the ACKED bytes, not the
//! base, after RECONFIGURE.
//!
//! Sized to the property (AGENTS.md): a small image, ONE seeded chunk, two
//! aligned device reads (the seeded chunk + an un-seeded chunk). No parked-I/O
//! timing, no long sleeps — the assertion is "the served bytes are the seeded
//! acked ones," which the least data demonstrates.
//!
//! Gating + run (mirrors `nbd_netlink_reconfigure.rs` — no FC needed):
//!
//! ```sh
//! sudo modprobe nbd nbds_max=4
//! sudo -E cargo test -p engram-host-agent --test nbd_verify_on_read \
//!     -- --ignored --nocapture
//! ```
//!
//! Self-skips when `/dev/nbdN` is missing or unwritable.
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use engram_chunk_store::cache::{ChunkCache, ChunkCacheConfig};
use engram_chunk_store::{ChunkStore, ManifestKind};
use engram_core::traits::BlobStorage;
use engram_core::types::manifest::ManifestRef;
use engram_host_agent::disk_daemon::{attach_manifest, reattach_manifest, NbdSlotAllocator};
use engram_storage_local::LocalBlobStorage;

/// Clear any stale binding from a prior aborted run (idempotent).
fn clear_stale_nbd_binding(nbd_path: &std::path::Path) {
    if let Some(idx) = nbd_path
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix("nbd"))
        .and_then(|s| s.parse::<u32>().ok())
    {
        let _ = engram_host_agent::disk_daemon::nbd_netlink::disconnect_device(idx);
    }
    // The kernel processes NBD_CMD_DISCONNECT asynchronously; the shared
    // /dev/nbd0 stays configured for a beat after `disconnect_device` returns.
    // A preceding FC test in this serial device sequence leaves it bound, so a
    // bare disconnect-then-CONNECT races into EBUSY. Wait on the same
    // /sys/block/nbdN/pid free signal `recover_stuck_nbd_devices` uses (bounded
    // — a genuinely stuck device still surfaces as the CONNECT error).
    if let Some(name) = nbd_path.file_name().and_then(|s| s.to_str()) {
        let pid_path = format!("/sys/block/{name}/pid");
        for _ in 0..100 {
            match std::fs::read_to_string(&pid_path) {
                Ok(s) if s.trim().is_empty() => break,
                Err(_) => break, // absent ⇒ device not bound
                Ok(_) => std::thread::sleep(std::time::Duration::from_millis(50)),
            }
        }
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

/// O_DIRECT pread so every byte travels the NBD wire (a page-cache hit could
/// otherwise fake "the device serves the right bytes"). Offset/len 4096-aligned.
fn pread_direct(path: &std::path::Path, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    assert_eq!(offset % 4096, 0);
    assert_eq!(len % 4096, 0);
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)?;
    let layout = std::alloc::Layout::from_size_align(len, 4096).unwrap();
    // SAFETY: non-zero layout; the buffer is fully written by pread before it
    // is read back; freed at the end of the function.
    let buf = unsafe { std::alloc::alloc(layout) };
    assert!(!buf.is_null());
    let rc = unsafe {
        libc::pread(
            f.as_raw_fd(),
            buf as *mut libc::c_void,
            len,
            offset as libc::off_t,
        )
    };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        unsafe { std::alloc::dealloc(buf, layout) };
        return Err(e);
    }
    let out = unsafe { std::slice::from_raw_parts(buf, rc as usize).to_vec() };
    unsafe { std::alloc::dealloc(buf, layout) };
    Ok(out)
}

/// A 16-byte stamp repeated across a block, so a read decodes which write
/// produced it (base vs the acked seed).
fn stamp(tag: u64) -> [u8; 16] {
    let mut s = [0u8; 16];
    s[..8].copy_from_slice(&tag.to_le_bytes());
    s[8..].copy_from_slice(&tag.to_le_bytes());
    s
}

#[tokio::test]
#[ignore]
async fn reattach_verify_on_read_serves_seeded_acked_bytes_not_base() {
    // Small budgets so any failure surfaces in seconds, not the prod 90/300s.
    std::env::set_var("ENGRAM_NBD_KERNEL_TIMEOUT_SECS", "5");
    std::env::set_var("ENGRAM_NBD_DEAD_CONN_TIMEOUT_SECS", "60");

    let nbd_path = match preflight() {
        Some(p) => p,
        None => return,
    };
    let work = tempfile::tempdir().expect("tempdir");

    // A small base image: every 4 KiB block tagged BASE. Small on purpose
    // (256 KiB) — the property is "acked bytes served, not base," which needs
    // only a couple of chunks.
    const BASE_TAG: u64 = 0;
    const ACKED_TAG: u64 = 0xAC_1D_ED; // recognizable "acked" content
    let image = work.path().join("disk.img");
    let mut bytes = vec![0u8; 256 * 1024];
    for block in bytes.chunks_mut(4096) {
        block[..16].copy_from_slice(&stamp(BASE_TAG));
    }
    std::fs::write(&image, &bytes).expect("write image");

    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(work.path().join("blob")));
    let store = Arc::new(ChunkStore::new(blob));
    let mut cache_cfg = ChunkCacheConfig::new(work.path().join("chunk-cache"));
    cache_cfg.budget_bytes = 64 * 1024 * 1024;
    let cache = ChunkCache::new(cache_cfg);
    let manifest = store
        .chunk_file(&image, ManifestKind::Disk, None)
        .await
        .expect("chunk image");
    let manifest_ref = ManifestRef::new();
    store
        .put_manifest(manifest_ref, &manifest)
        .await
        .expect("put manifest");

    // Generation one: CONNECT + serve.
    let pool = NbdSlotAllocator::from_paths(vec![nbd_path.clone()]).expect("pool");
    let state = attach_manifest(
        manifest_ref,
        cache.clone(),
        store.clone(),
        &pool,
        u64::MAX,
        /*fork=*/ false,
    )
    .await
    .expect("netlink CONNECT attach");
    let device = state.device_path().to_path_buf();
    let chunk_size = state.backend.chunk_size();
    assert!(chunk_size % 4096 == 0, "chunk size must be block-aligned");

    // The predecessor's acked-but-un-uploaded write: chunk index 1, a full
    // chunk stamped ACKED. This is what the shutdown spool would carry.
    let seed_idx = 1usize;
    let mut acked_chunk = vec![0u8; chunk_size as usize];
    for block in acked_chunk.chunks_mut(4096) {
        block[..16].copy_from_slice(&stamp(ACKED_TAG));
    }
    let seed_dirty = vec![(seed_idx, acked_chunk.clone())];

    // "Pod roll": the serve loop dies with no disconnect; the slot drops.
    let engram_host_agent::disk_daemon::NbdSandboxState {
        scheduler: _,
        backend: _gen1_backend,
        handle,
        slot,
    } = state;
    handle.abandon();
    drop(slot);

    // Generation two: claim the SAME device + RECONFIGURE, seeding the acked
    // chunk BEFORE the RECONFIGURE. `reattach_manifest` runs its in-process
    // verify-on-read internally; a mismatch would make this `Err`.
    let pool2 = NbdSlotAllocator::from_paths(vec![nbd_path.clone()]).expect("pool2");
    let slot2 = pool2.claim(&device).await.expect("claim survivor device");
    let state2 = reattach_manifest(
        manifest_ref,
        cache,
        store,
        slot2,
        u64::MAX,
        Some(seed_dirty),
    )
    .await
    .expect("RECONFIGURE reattach with seeded acked chunk (verify-on-read passed)");

    // The DEVICE-plane proof: an O_DIRECT read of the seeded chunk returns the
    // ACKED bytes, not the rolled-back base.
    let seeded_off = seed_idx as u64 * chunk_size;
    let seeded_read = pread_direct(&device, seeded_off, 4096).expect("seeded chunk device read");
    assert_eq!(
        &seeded_read[..16],
        &stamp(ACKED_TAG),
        "the seeded chunk must serve the acked bytes after RECONFIGURE, not the rolled-back base",
    );

    // An un-seeded chunk still serves base — the seed didn't smear across the
    // disk.
    let base_read = pread_direct(&device, 0, 4096).expect("base chunk device read");
    assert_eq!(
        &base_read[..16],
        &stamp(BASE_TAG),
        "an un-seeded chunk must still serve base content",
    );

    // Clean teardown so the device is free for the next run.
    drop(state2);
    tokio::time::sleep(Duration::from_millis(300)).await;
    clear_stale_nbd_binding(&device);
}
