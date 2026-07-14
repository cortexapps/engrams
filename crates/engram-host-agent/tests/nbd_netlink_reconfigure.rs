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

/// Containment (cascade fix, 2026-06-18): the NBD suite runs serially against a
/// single SHARED `/dev/nbd0`. A test that panics mid-flight (e.g. a timing
/// assertion) skips its teardown and leaves the device bound, so EVERY
/// subsequent test fails with "NBD attach failed" — one flake amplifies into
/// the whole suite. Clearing any stale binding at the START of each test
/// breaks that chain: a prior leak is reaped here, not inherited. Idempotent —
/// a no-op when nothing is bound (the ADR 0017 startup-recovery shape).
fn clear_stale_nbd_binding(nbd_path: &std::path::Path) {
    if let Some(idx) = nbd_path
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.strip_prefix("nbd"))
        .and_then(|s| s.parse::<u32>().ok())
    {
        let _ = engram_host_agent::disk_daemon::nbd_netlink::disconnect_device(idx);
    }
}

/// Reap a spawned child even when the test panics (so nextest doesn't flag a
/// LEAK and the process can't outlive the run). A bare `Command` child isn't
/// killed on unwind; this is.
struct KillOnDrop(std::process::Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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
    // Issue #582: this test is CI-flaky; wire up tracing so the next
    // natural failure captures reattach()/serve_loop diagnostics instead
    // of a bare assert. Mirrors `two_host_live_teleport.rs`'s init exactly
    // — `try_init` so a repeat init (another test in the same binary) is
    // harmless.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            "engram_host_agent=debug,engram_chunk_store=debug,engram_sandbox_firecracker=info",
        )
        .with_test_writer()
        .try_init();

    // Issue #582 root cause: the "parked" probe below is indistinguishable
    // (from userspace) from a read whose request went IN-FLIGHT on the
    // dying gen-1 socket during the abort transition. A queued-parked
    // request dispatches the moment RECONFIGURE lands; an in-flight-doomed
    // one requeues only after the kernel's PER-REQUEST timeout — which
    // attach sets to 90s (`nbd_kernel_timeout_secs`), triple the 30s
    // completion budget, so the test's outcome was decided by which side
    // of the dead-mark the probe's request landed on (load-dependent: 2
    // CI failures in 3 runs on 2026-07-06). Pin the request timeout small
    // for THIS test so both interleavings complete within the budget —
    // the asserted property (parked I/O survives RECONFIGURE with the
    // right bytes) is unchanged. Nextest runs each test in its own
    // process, so the env override cannot leak.
    std::env::set_var("ENGRAM_NBD_KERNEL_TIMEOUT_SECS", "5");

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

    let read1 = pread_direct(&device, 0, 4096).expect("gen-1 read");
    assert_eq!(&read1[..16], &bytes[..16], "gen-1 content mismatch");

    // Children spawned while the daemon serves (FC, in prod) must
    // NOT inherit the serve socket — a leaked fd keeps the kernel's
    // connection "alive" past the parent's death, so the dead-mark
    // only happens at the next 90s request timeout and the
    // successor's RECONFIGURE hits the kernel's silently-ACKed
    // ENOSPC (prod canary 2026-06-12). SOCK_CLOEXEC pins this; the
    // sleeper below would re-break it if that flag ever regresses.
    // KillOnDrop so a panic below (e.g. the dead-window assertion) reaps this
    // child during unwind instead of leaking it (the FAIL+LEAK nextest flagged,
    // which kept /dev/nbd0 busy and cascaded into the rest of the suite).
    let sleeper = KillOnDrop(
        std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("spawn fd-inheritance sleeper"),
    );

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

    // A read issued DURING the dead window must PARK, not EIO. But `abandon()`
    // aborts the serve task ASYNCHRONOUSLY (task.abort + mem::forget — it does
    // NOT await the socket close), so there's a brief transition after the
    // serve socket dies before the kernel marks the connection dead and starts
    // parking I/O. A read issued mid-transition EIOs — the source of the
    // 2.797s flake (green on the PR run, red under load on the main push).
    //
    // Poll instead of asserting on a single immediate read: a fresh O_DIRECT
    // read (offset past the first chunk so the cache can't satisfy it — it goes
    // to the KERNEL, which has no daemon) must transition to PARKED (still
    // pending after a 2s settle) within a bounded window. A read that keeps
    // completing/EIOing past the window is the real pre-netlink prod failure
    // (never parks), and still fails the test.
    let parked = {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let device_for_parked = device.clone();
            let probe = tokio::task::spawn_blocking(move || {
                pread_direct(&device_for_parked, 4 * 1024 * 1024, 4096)
            });
            tokio::time::sleep(Duration::from_secs(2)).await;
            if !probe.is_finished() {
                break probe; // parked under dead_conn_timeout — the asserted state
            }
            // Completed within the settle: during the abort→close transition the
            // read EIOs; once dead_conn parking is active it blocks. Reap the
            // finished probe and retry until the window closes.
            let _ = probe.await;
            assert!(
                std::time::Instant::now() < deadline,
                "reads never parked under dead_conn_timeout within 20s — the \
                 pre-netlink prod failure (EIO/complete instead of parking)",
            );
        }
    };

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
    // the next test run. `drop(sleeper)` reaps the child via KillOnDrop.
    drop(sleeper);
    drop(state2);
    tokio::time::sleep(Duration::from_millis(300)).await;
}

/// Shared mini-fixture for the identifier tests below: a 4 MiB tagged
/// image chunked into a fresh store. Small on purpose — the property
/// under test is the CONNECT/RECONFIGURE identifier contract, not
/// throughput.
async fn identifier_fixture(
    work: &std::path::Path,
) -> (
    Vec<u8>,
    engram_chunk_store::Manifest,
    ManifestRef,
    ChunkCache,
    Arc<ChunkStore>,
) {
    let image = work.join("disk.img");
    let mut bytes = vec![0u8; 4 * 1024 * 1024];
    for (i, block) in bytes.chunks_mut(4096).enumerate() {
        let tag = (i as u64).to_le_bytes();
        block[..8].copy_from_slice(&tag);
    }
    std::fs::write(&image, &bytes).expect("write image");
    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(work.join("blob")));
    let store = Arc::new(ChunkStore::new(blob));
    let mut cache_cfg = ChunkCacheConfig::new(work.join("chunk-cache"));
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
    (bytes, manifest, manifest_ref, cache, store)
}

/// 2026-07-13 dfa0face regression — the incident shape: a device is
/// CONNECTed under the attach-time manifest id, the session's disk
/// lineage forks (ADR 0077 — flushes publish under a private id), the
/// pod rolls, and the rehydrate arrives holding a ref whose manifest id
/// no longer matches the kernel's recorded identifier. Pre-fix,
/// `reattach_manifest` re-derived the identifier from the rehydrate ref
/// and the kernel's strcmp died with EINVAL — every forked-chain
/// survivor's disk stayed dead across every roll. The contract now:
/// RECONFIGURE echoes the kernel's own recorded connect-time identifier
/// (`/sys/block/nbdN/backend`), so the manifest id on the ref is
/// irrelevant to adoption.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Linux + modprobe nbd + writable /dev/nbd0 (root)"]
async fn reattach_echoes_kernel_identifier_across_manifest_fork() {
    let nbd_path = match preflight() {
        Some(p) => p,
        None => return,
    };
    let work = tempfile::tempdir().expect("tempdir");
    let (bytes, manifest, base_ref, cache, store) = identifier_fixture(work.path()).await;

    // Generation one: the FRESH-CREATE shape — connect under the base
    // ref, fork the manifest identity at attach.
    let pool = NbdSlotAllocator::from_paths(vec![nbd_path.clone()]).expect("pool");
    let state = attach_manifest(
        base_ref,
        cache.clone(),
        store.clone(),
        &pool,
        u64::MAX,
        /*fork=*/ true,
    )
    .await
    .expect("netlink CONNECT attach (forked)");
    let device = state.device_path().to_path_buf();
    let read1 = pread_direct(&device, 0, 4096).expect("gen-1 read");
    assert_eq!(&read1[..16], &bytes[..16], "gen-1 content mismatch");

    // The kernel durably recorded the connect-time identifier.
    let dev_name = device.file_name().unwrap().to_str().unwrap().to_string();
    let kernel_id = std::fs::read_to_string(format!("/sys/block/{dev_name}/backend"))
        .expect("kernel backend attr readable")
        .trim()
        .to_string();
    assert_eq!(
        kernel_id,
        base_ref.manifest_id.to_string(),
        "CONNECT must record the attach-time manifest id",
    );

    // The rehydrate ref carries a DIFFERENT manifest id (the forked live
    // chain in prod). Persist the content there like the flush scheduler
    // would have.
    let live_ref = ManifestRef::new();
    store
        .put_manifest(live_ref, &manifest)
        .await
        .expect("put live manifest");

    // Pod roll, then generation two rehydrates from the diverged ref.
    let engram_host_agent::disk_daemon::NbdSandboxState {
        scheduler: _,
        backend: _gen1_backend,
        handle,
        slot,
    } = state;
    handle.abandon();
    drop(slot);

    let pool2 = NbdSlotAllocator::from_paths(vec![nbd_path.clone()]).expect("pool2");
    let slot2 = pool2.claim(&device).await.expect("claim survivor device");
    let state2 = reattach_manifest(live_ref, cache, store, slot2, u64::MAX)
        .await
        .expect("RECONFIGURE must adopt regardless of the ref's manifest id (pre-fix: EINVAL)");

    // Adoption serves the right bytes, and the kernel identifier is
    // unchanged (RECONFIGURE never rewrites it).
    let read2 = pread_direct(&device, 2 * 1024 * 1024, 4096).expect("gen-2 read");
    assert_eq!(
        &read2[..16],
        &bytes[2 * 1024 * 1024..2 * 1024 * 1024 + 16],
        "gen-2 content mismatch",
    );
    let kernel_id_after = std::fs::read_to_string(format!("/sys/block/{dev_name}/backend"))
        .expect("kernel backend attr readable post-reattach")
        .trim()
        .to_string();
    assert_eq!(
        kernel_id_after, kernel_id,
        "RECONFIGURE must not rewrite the identifier"
    );
    drop(state2);
    tokio::time::sleep(Duration::from_millis(300)).await;
}
