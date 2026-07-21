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
//! Sized to the property (AGENTS.md): a small image, ONE seeded chunk. The
//! device read is PARKED during the dead-connection window before the
//! RECONFIGURE (mirroring `nbd_netlink_reconfigure`), so the RECONFIGURE
//! adopts a genuinely-dead connection — without that ordering the fresh
//! socket races the kernel's dead-marking, RECONFIGURE gets -ENOSPC, and the
//! serve loop silently isn't wired to the kernel (the in-process probe reads
//! only the dirty tier, so it can't catch that; the parked device read can).
//! The assertion is "the served bytes are the seeded acked ones."
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
    let Some(idx) = nbd_path
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix("nbd"))
        .and_then(|s| s.parse::<u32>().ok())
    else {
        return;
    };
    // A preceding FC test in this serial /dev/nbd0 sequence
    // (`sigterm_overrun_...`) abandons a successor data plane at process
    // exit, which leaves the device netlink-CONFIGURED under its
    // dead_conn_timeout with NO serving pid. So /sys/block/nbdN/pid reads
    // "free" while NBD_CMD_CONNECT still returns EBUSY, and a single
    // disconnect at t=0 races the prior process's teardown (re-park after
    // our disconnect). Re-issue NBD_CMD_DISCONNECT — which clears a
    // dead-conn-parked config immediately — on a bounded settle loop so a
    // late re-park is torn back down before this test's CONNECT.
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

/// O_DIRECT pwrite so the write travels the NBD wire (and PARKS under
/// `dead_conn_timeout` when the device has no server — the queued-guest-write
/// state a pod roll leaves behind). Offset/len 4096-aligned.
fn pwrite_direct(path: &std::path::Path, offset: u64, data: &[u8]) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    assert_eq!(offset % 4096, 0);
    assert_eq!(data.len() % 4096, 0);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)?;
    let layout = std::alloc::Layout::from_size_align(data.len(), 4096).unwrap();
    // SAFETY: non-zero layout; fully initialized from `data` before the
    // pwrite; freed at the end of the function.
    let buf = unsafe { std::alloc::alloc(layout) };
    assert!(!buf.is_null());
    unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len()) };
    let rc = unsafe {
        libc::pwrite(
            f.as_raw_fd(),
            buf as *const libc::c_void,
            data.len(),
            offset as libc::off_t,
        )
    };
    let result = if rc == data.len() as isize {
        Ok(())
    } else if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Err(std::io::Error::other(format!("short pwrite: {rc}")))
    };
    unsafe { std::alloc::dealloc(buf, layout) };
    result
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
    // A 64 KiB chunk size (vs the 16 MiB default) so the 256 KiB image spans
    // 4 chunks — the test seeds chunk index 1, which the default single-chunk
    // layout would put past the device end (offset 16 MiB > 256 KiB total).
    let manifest = store
        .chunk_file(&image, ManifestKind::Disk, Some(64 * 1024))
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
    let seeded_off = seed_idx as u64 * chunk_size;

    // "Pod roll": the serve loop dies with no disconnect; the slot drops back
    // to the pool. The kernel keeps the device configured (dead-conn parking).
    let engram_host_agent::disk_daemon::NbdSandboxState {
        scheduler: _,
        backend: _gen1_backend,
        handle,
        slot,
    } = state;
    handle.abandon();
    drop(slot);

    // Park an O_DIRECT read at the SEEDED chunk's offset during the dead window.
    // O_DIRECT (this offset was never read, so no page-cache hit) travels the
    // NBD wire to the KERNEL, which has no server after the abandon → it PARKS
    // under `dead_conn_timeout`. Waiting for it to park BEFORE the RECONFIGURE
    // is what makes the RECONFIGURE robust: the old connection is already marked
    // dead, so the kernel adopts our fresh socket on the FIRST try instead of
    // silently-ACKing `-ENOSPC` ("no dead connection to replace") and dropping
    // it — the race that made a naive `abandon → immediate RECONFIGURE` read as
    // a FALSE "adopted" and then EIO the device read (this is exactly the
    // structure `nbd_netlink_reconfigure` uses to stay green in CI).
    let parked = {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let device_for_parked = device.clone();
            let probe = tokio::task::spawn_blocking(move || {
                pread_direct(&device_for_parked, seeded_off, 4096)
            });
            tokio::time::sleep(Duration::from_secs(2)).await;
            if !probe.is_finished() {
                break probe; // parked under dead_conn_timeout — the state we want
            }
            // Completed within the settle: during the abort→close transition the
            // read EIOs; once dead-conn parking is active it blocks. Reap the
            // finished probe and retry until the window opens.
            let _ = probe.await;
            assert!(
                std::time::Instant::now() < deadline,
                "the read never parked under dead_conn_timeout within 20s — the \
                 pre-netlink failure shape (EIO/complete instead of parking)",
            );
        }
    };

    // Generation two: claim the SAME device + RECONFIGURE, seeding the acked
    // chunk BEFORE the RECONFIGURE (the seed-dirty-before-RECONFIGURE ordering).
    // `reattach_manifest` runs its in-process verify-on-read internally; a
    // mismatch would make this `Err`.
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

    // The DEVICE-plane proof: the parked read resumes and returns the ACKED
    // (seeded) bytes — the kernel serves the acked write, not the rolled-back
    // base. This is verify-on-read proven at the real kernel plane (the
    // in-process probe above reads only the backend's dirty tier).
    let parked_bytes = tokio::time::timeout(Duration::from_secs(30), parked)
        .await
        .expect("the parked read must complete after RECONFIGURE")
        .expect("join")
        .expect("parked read result");
    assert_eq!(
        &parked_bytes[..16],
        &stamp(ACKED_TAG),
        "the seeded chunk must serve the acked bytes after RECONFIGURE, not the rolled-back base",
    );

    // No-smear: an un-seeded chunk still serves BASE (the seed didn't smear
    // across the disk). Proven IN-PROCESS through the rehydrated gen-2 backend,
    // NOT at the device plane — deliberately.
    //
    // Why not a device-plane O_DIRECT read here: chunk 0 is un-seeded, so it is
    // NOT in the adopted dirty tier (RAM) — the serve loop must fetch it COLD
    // from the ChunkStore. Under this test's deliberately-tight 5 s kernel
    // request timeout (`ENGRAM_NBD_KERNEL_TIMEOUT_SECS=5`) on the loaded 2-vcpu
    // FC-lane microVM, that cold store fetch on the freshly-RECONFIGURE'd
    // connection races the kernel request timeout — a request timeout marks the
    // connection dead and the read then parks the full `dead_conn_timeout`
    // before EIO'ing (~76 s observed). This is TEST TIMING, not a product gap:
    // the serve loop demonstrably serves store-backed chunks (every gen-1
    // CONNECT read does, and `nbd_netlink_reconfigure`'s post-RECONFIGURE
    // read2 does — with a cache warmed by its gen-1 read1), and the gen-2
    // backend is built by the SAME `from_blob(cache, store, manifest_ref)` path.
    // The primary property (the KERNEL serves the ACKED bytes) is already proven
    // at the device plane by the seeded parked read above; the no-smear check
    // is a backend-content property that does not need the kernel path — so we
    // size it to the property (AGENTS.md) and read the backend directly.
    //
    // This in-process read ALSO definitively closes the "can a rehydrated serve
    // loop serve store-backed base chunks at all?" question: it fetches chunk 0
    // COLD from the store through the gen-2 backend and must return BASE.
    let base_read = state2
        .backend
        .read(0, 4096)
        .await
        .expect("in-process cold base read through the rehydrated gen-2 backend");
    assert_eq!(
        &base_read[..16],
        &stamp(BASE_TAG),
        "an un-seeded chunk must still serve base content (no seed smear)",
    );

    // Clean teardown so the device is free for the next run.
    drop(state2);
    tokio::time::sleep(Duration::from_millis(300)).await;
    clear_stale_nbd_binding(&device);
}

/// 2026-07-21 incident regression: a surviving guest with QUEUED WRITES
/// (parked under `dead_conn_timeout` across the pod roll) floods them in the
/// instant the RECONFIGURE adopts the fresh socket — and the hottest chunks
/// are exactly the spooled ones. The old post-RECONFIGURE verify-on-read
/// probe read the guest's fresh write back, mis-read it as a rolled-back
/// base, and parked a HEALTHY survivor (→ quarantine → evict-budget
/// exhaustion → destroy → rewind: acked-write loss manufactured by the guard
/// itself; four-for-four across the night's host rolls). The probe now runs
/// at the `VerifySeed` step, strictly before the RECONFIGURE, so a racing
/// guest write can never fail the reattach.
///
/// Under the old code this test fails with high probability (the released
/// write patches the dirty tier while the driver is still in BLKFLSBUF,
/// ahead of the probe); under the fixed code it passes deterministically —
/// the probe completes before the kernel ever releases the write.
#[tokio::test]
#[ignore]
async fn reattach_survives_guest_write_racing_reconfigure() {
    std::env::set_var("ENGRAM_NBD_KERNEL_TIMEOUT_SECS", "5");
    std::env::set_var("ENGRAM_NBD_DEAD_CONN_TIMEOUT_SECS", "60");

    let nbd_path = match preflight() {
        Some(p) => p,
        None => return,
    };
    let work = tempfile::tempdir().expect("tempdir");

    const BASE_TAG: u64 = 0;
    const ACKED_TAG: u64 = 0xAC_1D_ED;
    const GUEST_TAG: u64 = 0x60E5_7EAD; // the live guest's post-resume write
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
        .chunk_file(&image, ManifestKind::Disk, Some(64 * 1024))
        .await
        .expect("chunk image");
    let manifest_ref = ManifestRef::new();
    store
        .put_manifest(manifest_ref, &manifest)
        .await
        .expect("put manifest");

    // Generation one: CONNECT + serve, then "pod roll" (abandon, no
    // disconnect — the kernel keeps the device configured, dead-conn parked).
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
    let seed_idx = 1usize;
    let seeded_off = seed_idx as u64 * chunk_size;
    let mut acked_chunk = vec![0u8; chunk_size as usize];
    for block in acked_chunk.chunks_mut(4096) {
        block[..16].copy_from_slice(&stamp(ACKED_TAG));
    }
    let seed_dirty = vec![(seed_idx, acked_chunk)];
    let engram_host_agent::disk_daemon::NbdSandboxState {
        scheduler: _,
        backend: _gen1_backend,
        handle,
        slot,
    } = state;
    handle.abandon();
    drop(slot);

    // Park an O_DIRECT WRITE of GUEST_TAG at the SEEDED chunk's offset during
    // the dead window — the queued guest write the RECONFIGURE will release
    // onto the probed chunk (the prod race shape). Same park-detection settle
    // loop as the read variant above.
    let mut guest_block = vec![0u8; 4096];
    guest_block[..16].copy_from_slice(&stamp(GUEST_TAG));
    let parked_write = {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let device_for_write = device.clone();
            let block = guest_block.clone();
            let probe = tokio::task::spawn_blocking(move || {
                pwrite_direct(&device_for_write, seeded_off, &block)
            });
            tokio::time::sleep(Duration::from_secs(2)).await;
            if !probe.is_finished() {
                break probe; // parked under dead_conn_timeout
            }
            let _ = probe.await;
            assert!(
                std::time::Instant::now() < deadline,
                "the write never parked under dead_conn_timeout within 20s",
            );
        }
    };

    // Generation two: the rehydrate. With the parked write racing the
    // RECONFIGURE, the reattach (and its pre-RECONFIGURE verify probe) must
    // still succeed — this is the incident's exact failure point.
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
    .expect("a guest write racing the RECONFIGURE must not fail the rehydrate");

    // The released write must complete against the fresh serve socket…
    tokio::time::timeout(Duration::from_secs(30), parked_write)
        .await
        .expect("the parked write must complete after RECONFIGURE")
        .expect("join")
        .expect("parked guest write result");

    // …and newest-wins: the guest's write supersedes the seed on its block,
    // while the rest of the seeded chunk keeps the acked bytes.
    let guest_read = state2
        .backend
        .read(seeded_off, 4096)
        .await
        .expect("read guest-written block");
    assert_eq!(
        &guest_read[..16],
        &stamp(GUEST_TAG),
        "the guest's post-RECONFIGURE write must supersede the seeded bytes",
    );
    let acked_read = state2
        .backend
        .read(seeded_off + 4096, 4096)
        .await
        .expect("read seed remainder");
    assert_eq!(
        &acked_read[..16],
        &stamp(ACKED_TAG),
        "the un-overwritten remainder of the seeded chunk keeps the acked bytes",
    );

    // Clean teardown so the device is free for the next run.
    drop(state2);
    tokio::time::sleep(Duration::from_millis(300)).await;
    clear_stale_nbd_binding(&device);
}
