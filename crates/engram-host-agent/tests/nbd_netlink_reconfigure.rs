//! ADR 0044 K2: the NBD survivor data plane across a host-agent
//! "death" — netlink CONNECT, dead-connection parking, RECONFIGURE.
//!
//! The pod-roll contract this pins:
//!
//!  1. A chunked disk attaches via netlink `NBD_CMD_CONNECT` and
//!     serves byte-identical content.
//!  2. The configuring process "dies" ([`NbdHandle::abandon`] — the
//!     serve socket closes with NO disconnect, exactly what the
//!     kernel sees when the host-agent is SIGKILLed). The device
//!     stays configured; I/O issued during the gap PARKS under
//!     `NBD_ATTR_DEAD_CONN_TIMEOUT` instead of failing with EIO
//!     (the pre-netlink behavior — prod 2026-06-11 — was an errored
//!     guest rootfs).
//!  3. A successor (the next host-agent generation) claims the SAME
//!     device and hands the kernel a fresh socket via
//!     `NBD_CMD_RECONFIGURE` ([`reattach_manifest`]); the parked
//!     read completes with correct bytes.
//!
//! Gating + run (mirrors `nbd_chunked_disk.rs` — no FC needed):
//!
//! ```sh
//! sudo modprobe nbd nbds_max=4
//! sudo -E cargo test -p engram-host-agent --test nbd_netlink_reconfigure \
//!     -- --ignored --nocapture
//! ```
//!
//! Self-skips when `/dev/nbdN` is missing or unwritable (Blacksmith
//! runners ship no nbd.ko; this runs on the dev-vm + any nbd-capable
//! Linux).

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
        Ok(_) => Some(nbd_path),
        Err(e) => {
            eprintln!(
                "SKIP: cannot open {} R/W: {e} — run as root (`sudo -E`)",
                nbd_path.display()
            );
            None
        }
    }
}

/// O_DIRECT pread so every byte travels the NBD wire — without it a
/// page-cache hit would fake "the device works" after the server is
/// gone. Buffer/offset/len all 4096-aligned per the O_DIRECT
/// contract.
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
    // SAFETY: layout is non-zero; allocation checked below; the
    // buffer is fully written by pread before being read back.
    let ptr = unsafe { std::alloc::alloc(layout) };
    assert!(!ptr.is_null());
    let rc = unsafe { libc::pread(f.as_raw_fd(), ptr.cast(), len, offset as i64) };
    let out = if rc == len as isize {
        let mut v = vec![0u8; len];
        // SAFETY: kernel wrote exactly `len` bytes at ptr.
        unsafe { std::ptr::copy_nonoverlapping(ptr, v.as_mut_ptr(), len) };
        Ok(v)
    } else if rc < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Err(std::io::Error::other(format!("short read: {rc} of {len}")))
    };
    // SAFETY: same layout as the alloc.
    unsafe { std::alloc::dealloc(ptr, layout) };
    out
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Linux + modprobe nbd + writable /dev/nbd0 (root)"]
async fn survivor_reconfigure_resumes_parked_io() {
    let nbd_path = match preflight() {
        Some(p) => p,
        None => return,
    };
    // Short dead-conn window so a failure mode (parked read NEVER
    // completing) fails the test in seconds, not the prod 300s.
    std::env::set_var("ENGRAM_NBD_DEAD_CONN_TIMEOUT_SECS", "60");

    let work = tempfile::tempdir().expect("tempdir");

    // A recognizable 8 MiB image: each 4 KiB block tagged with its
    // own index so any cross-offset confusion is caught.
    let image = work.path().join("disk.img");
    let mut bytes = vec![0u8; 8 * 1024 * 1024];
    for (i, block) in bytes.chunks_mut(4096).enumerate() {
        let tag = (i as u64).to_le_bytes();
        block[..8].copy_from_slice(&tag);
        block[8..16].copy_from_slice(&tag);
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

    // 1. Generation one: netlink CONNECT + serve.
    let pool = NbdSlotAllocator::from_paths(vec![nbd_path.clone()]).expect("pool");
    let state = attach_manifest(manifest_ref, cache.clone(), store.clone(), &pool, u64::MAX)
        .await
        .expect("netlink CONNECT attach");
    let device = state.device_path().to_path_buf();

    let read1 = pread_direct(&device, 0, 4096).expect("gen-1 read");
    assert_eq!(&read1[..16], &bytes[..16], "gen-1 content mismatch");

    // Children spawned while the daemon serves (FC, in prod) must
    // NOT inherit the serve socket — a leaked fd keeps the kernel's
    // connection "alive" past the parent's death, so the dead-mark
    // only happens at the next 90s request timeout and the
    // successor's RECONFIGURE hits the kernel's silently-ACKed
    // ENOSPC (prod canary 2026-06-12). SOCK_CLOEXEC pins this; the
    // sleeper below would re-break it if that flag ever regresses.
    let mut sleeper = std::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("spawn fd-inheritance sleeper");

    // 2. "Pod roll": the serve loop dies without a disconnect. The
    //    slot lease drops back to the pool (in prod the new process
    //    builds a fresh pool; same shape).
    let engram_host_agent::disk_daemon::NbdSandboxState {
        scheduler: _,
        backend: _gen1_backend,
        handle,
        slot,
    } = state;
    handle.abandon();
    drop(slot);

    // A read issued DURING the dead window must PARK, not EIO. Run
    // it on a blocking thread; it should still be pending after a
    // couple of seconds.
    let device_for_parked = device.clone();
    let parked = tokio::task::spawn_blocking(move || {
        // An offset past the first chunk so the chunk cache can't
        // satisfy it without the daemon... (the read goes to the
        // KERNEL, which has no daemon — the cache layer is never
        // reached; O_DIRECT additionally defeats the page cache).
        pread_direct(&device_for_parked, 4 * 1024 * 1024, 4096)
    });
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !parked.is_finished(),
        "read during the dead window must park under dead_conn_timeout, \
         not complete or fail (EIO here = the pre-netlink prod failure)",
    );

    // 3. Generation two: claim the SAME device + RECONFIGURE.
    let pool2 = NbdSlotAllocator::from_paths(vec![nbd_path.clone()]).expect("pool2");
    let slot2 = pool2.claim(&device).await.expect("claim survivor device");
    let state2 = reattach_manifest(manifest_ref, cache, store, slot2, u64::MAX)
        .await
        .expect("netlink RECONFIGURE reattach");

    // The parked read completes with the right bytes.
    let parked_bytes = tokio::time::timeout(Duration::from_secs(30), parked)
        .await
        .expect("parked read must complete after RECONFIGURE")
        .expect("join")
        .expect("parked read result");
    let want_off = 4 * 1024 * 1024;
    assert_eq!(
        &parked_bytes[..16],
        &bytes[want_off..want_off + 16],
        "parked read returned wrong bytes after RECONFIGURE",
    );

    // Fresh reads through the new generation are byte-identical too.
    let read2 = pread_direct(&device, 2 * 1024 * 1024, 4096).expect("gen-2 read");
    assert_eq!(
        &read2[..16],
        &bytes[2 * 1024 * 1024..2 * 1024 * 1024 + 16],
        "gen-2 content mismatch",
    );

    // Clean teardown (netlink disconnect) so the device is free for
    // the next test run.
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    drop(state2);
    tokio::time::sleep(Duration::from_millis(300)).await;
}
