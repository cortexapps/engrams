//! Phase 4 end-to-end NBD integration test.
//!
//! Boots a real Firecracker microVM whose rootfs is `/dev/nbd0`,
//! served by the chunked NBD daemon. Asserts the guest reads back
//! byte-identical content (via the marker-conditional init —
//! `prepare_verified_rootfs`; the raw test rootfs has no agentd, so
//! vsock exec is unavailable) AND the daemon shuts down cleanly on
//! `destroy()`.
//!
//! Gating + run (mirrors `snapshot_uffd.rs`):
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! sudo modprobe nbd nbds_max=4
//! sudo chmod 666 /dev/nbd0
//! cargo test -p engram-host-agent --test nbd_chunked_disk \
//!     -- --ignored --nocapture
//! ```
//!
//! `sudo chmod 666 /dev/nbd0` used to suffice; newer Ubuntu kernels
//! (observed on the dev-vm's 6.x, 2026-06) gate NBD configuration
//! (netlink and ioctl alike) behind CAP_SYS_ADMIN regardless of the
//! device's permission bits, so these tests now need to run under
//! `sudo -E` (mirroring the root-required FC tests in
//! `run-boot-test.sh`). Production is unaffected — the host-agent
//! runs as root.
//!
//! What this test covers that the pure-Rust e2e suite
//! (`adr_0007_e2e.rs`) doesn't:
//!  - The netlink NBD orchestration with the actual Linux kernel
//!    (`NBD_CMD_CONNECT` with size/flags/timeouts/socket attrs —
//!    ADR 0044 K2 replaced the legacy `NBD_SET_SOCK`/`NBD_DO_IT`
//!    ioctls so the data plane survives host-agent restarts).
//!  - The kernel's NBD transmission protocol talking to our daemon's
//!    `serve_loop` (request headers, response headers, payload).
//!  - FC's virtio-blk frontend reading from `/dev/nbd0` and the guest
//!    kernel mounting it as the rootfs.
//!  - Clean tear-down: netlink `NBD_CMD_DISCONNECT` releases the
//!    device and the slot returns to the pool.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use engram_chunk_store::cache::{ChunkCache, ChunkCacheConfig};
use engram_chunk_store::{ChunkStore, ManifestKind};
use engram_core::traits::{BlobStorage, SandboxBackend};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_host_agent::disk_daemon::{attach_manifest, NbdSlotAllocator};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, RestoreMode};
use engram_storage_local::LocalBlobStorage;

/// Pre-flight: KVM + firecracker + the test rootfs path. Mirrors
/// the FC crate's `common::fc_preflight` but inlined here so we
/// don't take a tests-only path dep on a sibling crate's private
/// helper module.
fn preflight() -> Option<(PathBuf, PathBuf, PathBuf)> {
    let kernel = match std::env::var("FC_TEST_KERNEL") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: FC_TEST_KERNEL not set; run fetch-fc-test-artifacts.sh");
            return None;
        }
    };
    let rootfs = match std::env::var("FC_TEST_ROOTFS") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: FC_TEST_ROOTFS not set");
            return None;
        }
    };
    if !std::path::Path::new("/dev/kvm").exists() {
        eprintln!("SKIP: /dev/kvm not present");
        return None;
    }
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
    // Check the daemon can open the device — fails-fast if perms
    // aren't right rather than letting the daemon error opaquely.
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&nbd_path)
    {
        Ok(_) => {}
        Err(e) => {
            eprintln!(
                "SKIP: cannot open {} R/W: {e} — run `sudo chmod 666 {}` or run this test as root",
                nbd_path.display(),
                nbd_path.display()
            );
            return None;
        }
    }
    let firecracker = match std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join("firecracker"))
            .find(|p| p.is_file())
    }) {
        Some(p) => p,
        None => {
            eprintln!("SKIP: firecracker not on $PATH");
            return None;
        }
    };
    let _ = firecracker; // not actually invoked here; FC backend resolves on its own
                         // Both tests verify guest-side content via a debugfs-injected
                         // marker-conditional init (see `prepare_verified_rootfs`).
    if !std::process::Command::new("debugfs")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("SKIP: debugfs (e2fsprogs) not available");
        return None;
    }
    Some((kernel, rootfs, nbd_path))
}

/// The sentinel only the prepared image carries; the guest's init
/// keeps the VM alive iff it reads back byte-identical.
const SENTINEL: &str = "survived-the-host-roll";

/// How long the guest gets to boot + run verify-init before the
/// host-side liveness assertion. A panicked guest (`panic=1 reboot=k`)
/// exits FC well inside this.
const VERIFY_GRACE: Duration = Duration::from_secs(6);

/// Copy `rootfs_src` into `work`, inject the sentinel file + a
/// marker-conditional init (via debugfs — no mount, no root needed for
/// the injection itself), and return the prepared image path.
///
/// Verification model shared by both tests: the guest's pid-1 script
/// reads `/session-work.txt`; on a byte-identical match it sleeps
/// forever, otherwise it exits — and a dying init panics the kernel
/// (`panic=1 reboot=k`), which exits FC. So "the sandbox is still in
/// `list()` after [`VERIFY_GRACE`]" IS the content assertion. The raw
/// FC-CI Ubuntu rootfs ships no agentd, so vsock exec — the previous
/// verification — structurally can't work here (it rotted unnoticed
/// because CI's NBD step self-skips on Blacksmith).
fn prepare_verified_rootfs(work: &std::path::Path, rootfs_src: &std::path::Path) -> PathBuf {
    let img = work.join("verified-rootfs.ext4");
    std::fs::copy(rootfs_src, &img).expect("copy rootfs");
    let marker = work.join("marker.txt");
    std::fs::write(&marker, format!("{SENTINEL}\n")).expect("write marker");
    // PATH is explicit because the kernel execs init with an empty
    // environment.
    let verify_init = work.join("verify-init.sh");
    std::fs::write(
        &verify_init,
        format!(
            "#!/bin/bash\n\
             export PATH=/usr/sbin:/usr/bin:/sbin:/bin\n\
             if [ \"$(cat /session-work.txt 2>/dev/null)\" = \"{SENTINEL}\" ]; then\n\
             \twhile true; do sleep 60; done\n\
             fi\n\
             exit 1\n"
        ),
    )
    .expect("write verify-init");
    for cmd in [
        format!("write {} /session-work.txt", marker.display()),
        format!("write {} /verify-init.sh", verify_init.display()),
        "sif /verify-init.sh mode 0100755".to_string(),
    ] {
        let out = std::process::Command::new("debugfs")
            .args(["-w", "-R", &cmd])
            .arg(&img)
            .output()
            .expect("debugfs");
        assert!(out.status.success(), "debugfs `{cmd}` failed");
    }
    img
}

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + modprobe nbd + writeable /dev/nbd0"]
async fn fc_microvm_boots_with_nbd_chunked_rootfs() {
    let (kernel, rootfs_src, nbd_path) = match preflight() {
        Some(v) => v,
        None => return,
    };

    let work = tempfile::tempdir().expect("tempdir");

    // 1. Stand up a chunk store + cache + plant the verified test
    //    rootfs (sentinel + marker-conditional init injected) as a
    //    chunked disk manifest.
    let rootfs_verified = prepare_verified_rootfs(work.path(), &rootfs_src);
    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(work.path().join("blob")));
    let store = Arc::new(ChunkStore::new(blob));
    let mut cache_cfg = ChunkCacheConfig::new(work.path().join("chunk-cache"));
    cache_cfg.budget_bytes = 256 * 1024 * 1024;
    let cache = ChunkCache::new(cache_cfg);

    let manifest = store
        .chunk_file(&rootfs_verified, ManifestKind::Disk, None)
        .await
        .expect("chunk test rootfs");
    let manifest_ref = ManifestRef::new();
    store
        .put_manifest(manifest_ref, &manifest)
        .await
        .expect("put rootfs manifest");
    eprintln!(
        "PLANT: chunked {} into {} chunks ({} MiB)",
        rootfs_verified.display(),
        manifest.chunks.len(),
        manifest.total_bytes / (1024 * 1024)
    );

    // 2. Allocate /dev/nbd0 from the pool + spawn the daemon
    //    serving the just-chunked manifest. The returned
    //    NbdSandboxState owns the live daemon + slot lease; drop
    //    cleanly tears down.
    let pool = NbdSlotAllocator::from_paths(vec![nbd_path.clone()]).expect("build slot pool");
    // ADR 0016 Phase B: threshold-notify wiring is irrelevant for
    // this NBD-only test — pass `u64::MAX` to disable. The Phase B
    // scheduler is also not installed (`scheduler: None`); the test
    // only exercises the NBD wire format + kernel binding.
    let nbd_state = attach_manifest(
        manifest_ref,
        cache,
        store.clone(),
        &pool,
        u64::MAX,
        /*fork=*/ false,
    )
    .await
    .expect("spawn NBD daemon against /dev/nbd0");
    let nbd_device = nbd_state.device_path().to_path_buf();
    eprintln!(
        "READY: NBD daemon serving manifest {} as {}",
        manifest_ref,
        nbd_device.display()
    );

    // Give the kernel a moment to register the device size after
    // NBD_SET_SOCK + NBD_DO_IT raced into the serve loop. A
    // sub-second wait is enough; we poll the BLKGETSIZE-equivalent
    // via std::fs::metadata to make sure /dev/nbdN reports >0
    // before FC tries to attach it.
    let nbd_ready_deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let meta = std::fs::metadata(&nbd_device);
        if meta.is_ok() {
            break;
        }
        if std::time::Instant::now() > nbd_ready_deadline {
            panic!(
                "NBD device {} did not become readable",
                nbd_device.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 3. Boot FC with rootfs_source = /dev/nbd0. FC's virtio-blk
    //    frontend reads from the NBD-backed block device; the
    //    kernel inside the guest sees a normal ext4 rootfs.
    let mut cfg = FirecrackerConfig::with_kernel(kernel);
    cfg.net_pool = None;
    cfg.restore_mode = RestoreMode::File;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/verify-init.sh".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "fc-nbd-test".into(),
        rootfs_source: Some(nbd_device.clone()),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 128 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
    };

    // FC backend assumes spec.rootfs_source is a regular file by
    // default; /dev/nbd0 is a block device, which FC's create
    // sometimes rejects. We need the test to surface the real
    // restriction loud rather than time out. The test runner sees
    // an explicit error path either way.
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    let create_result = backend.create(spec).await;
    let sandbox_id = match create_result {
        Ok(id) => id,
        Err(e) => {
            // The FC backend currently runs an `is_dir()` check on
            // rootfs_source. A device file passes that check, but
            // FC's `put_drive` may still reject `/dev/nbdN`
            // depending on the running version. Surface the error
            // verbatim — the dev VM operator can decide whether to
            // patch FC's backend or live with materialize-to-file.
            // The teardown order matters: drop state BEFORE panic
            // so the NBD slot returns and the daemon's tokio task
            // doesn't leak.
            drop(nbd_state);
            panic!(
                "FirecrackerBackend.create against /dev/nbd0 failed: {e}\n\
                 (this can happen on FC versions that mmap rootfs files; \
                 fall back to materialize-to-file via PooledBackend then.)"
            );
        }
    };
    eprintln!("BOOTED: FC sandbox {} on NBD-backed rootfs", sandbox_id);

    // 4. Liveness-as-content-assertion (see `prepare_verified_rootfs`):
    //    the guest's verify-init only stays alive on a byte-identical
    //    sentinel read off the NBD-served disk.
    tokio::time::sleep(VERIFY_GRACE).await;
    let live = backend.list().await.expect("list");
    assert!(
        live.contains(&sandbox_id),
        "NBD-backed VM died within the grace period — the guest's \
         verify-init found no byte-identical /session-work.txt \
         (chunk corruption, mount failure, or NBD wire fault)",
    );
    eprintln!("VERIFIED: guest alive at +{VERIFY_GRACE:?} — sentinel read back byte-identical");

    // 5. Tear down. destroy() inside FC backend kills the VM;
    //    dropping nbd_state disconnects from the kernel + releases
    //    the slot. The Drop order here exercises: VM down → daemon
    //    down → slot returned.
    backend
        .destroy(sandbox_id)
        .await
        .expect("destroy NBD-backed sandbox");
    drop(nbd_state);
    eprintln!("TEARDOWN: VM destroyed + NBD daemon disconnected");
}

/// ADR 0028 Fix B end-to-end (host half of the `cf4d4afd` shape):
/// a session's EVOLVED rootfs lineage exists only as a chunked
/// manifest in blob storage (the continuous-sync `live_disk_manifest`)
/// — no memory snapshot, no sidecar, no image bundle on this "peer"
/// host. Recovery = `PooledBackend::create` with
/// `spec.rootfs_manifest = Some(manifest)`:
///
///   - the pooled backend NBD-attaches THAT manifest (not an image's),
///   - FC fresh-boots a kernel that mounts the evolved rootfs,
///   - the guest proves it's the evolved disk (not a base) via a
///     marker-conditional init: it reads back the marker file only
///     the "session" wrote and stays alive iff the content matches —
///     a mismatch exits init, which panics the kernel (`panic=1
///     reboot=k`) and kills the VM. Host-side liveness after a grace
///     period IS the content assertion. (No exec: the raw test rootfs
///     carries no agentd, so vsock exec can't work here.)
///   - `destroy()` tears the daemon down cleanly.
#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + modprobe nbd + writeable /dev/nbd0 + debugfs"]
async fn disk_only_cold_boot_via_rootfs_manifest_override() {
    let (kernel, rootfs_src, nbd_path) = match preflight() {
        Some(v) => v,
        None => return,
    };

    let work = tempfile::tempdir().expect("tempdir");

    // 1. Build the "session's evolved disk": the verified rootfs's
    //    sentinel file is the stand-in for the work a real session
    //    wrote before its host died (only the evolved lineage carries
    //    it — a base image boot would fail verify-init).
    let evolved = prepare_verified_rootfs(work.path(), &rootfs_src);

    // 2. Chunk the evolved disk — this manifest IS the
    //    live_disk_manifest a coord would hand the recovery.
    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(work.path().join("blob")));
    let store = ChunkStore::new(blob);
    let manifest = store
        .chunk_file(&evolved, ManifestKind::Disk, None)
        .await
        .expect("chunk evolved rootfs");
    let manifest_ref = ManifestRef::new();
    store
        .put_manifest(manifest_ref, &manifest)
        .await
        .expect("put evolved manifest");
    // The evolved file itself never travels — only its chunks. Remove
    // it so nothing can accidentally read it directly.
    std::fs::remove_file(&evolved).expect("rm evolved rootfs");

    // 3. Stand up the "peer host": PooledBackend over FC with the NBD
    //    pool + chunk store + cache wired, but NO image cache — the
    //    recovery must not need the image's bytes at all.
    let mut cfg = FirecrackerConfig::with_kernel(kernel);
    cfg.net_pool = None;
    cfg.restore_mode = RestoreMode::File;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/verify-init.sh".into();
    let inner = Arc::new(FirecrackerBackend::new(work.path(), cfg));
    let pool = NbdSlotAllocator::from_paths(vec![nbd_path]).expect("slot pool");
    let mut cache_cfg = ChunkCacheConfig::new(work.path().join("chunk-cache"));
    cache_cfg.budget_bytes = 256 * 1024 * 1024;
    let pooled = engram_host_agent::pooled_backend::PooledBackend::new(inner)
        .with_nbd_pool(pool)
        .with_chunk_store(store, work.path().join("materialize"))
        .with_chunk_cache(ChunkCache::new(cache_cfg));

    // 4. The cold-boot recovery spec — what
    //    `coordinator::evacuation::evacuate_dead_source` builds via
    //    `cold_boot_spec(...)` + the rootfs_manifest override.
    let spec = SandboxSpec {
        image: "fc-disk-only-recovery-test".into(),
        rootfs_source: None,
        image_uri: None,
        rootfs_manifest: Some(manifest_ref),
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 128 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
    };
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");
    let sandbox_id = pooled
        .create(spec)
        .await
        .expect("disk-only cold boot via rootfs_manifest override");
    eprintln!("BOOTED: fresh kernel on the evolved rootfs lineage, sandbox {sandbox_id}");

    // 5. The marker only exists on the EVOLVED disk, and the
    //    verify-init keeps the guest alive only on a byte-identical
    //    read-back. Surviving the grace period IS the content
    //    assertion: a base disk (no marker), a corrupted chunk, or a
    //    failed mount all panic the guest and empty `list()`.
    tokio::time::sleep(VERIFY_GRACE).await;
    let live = pooled.list().await.expect("list");
    assert!(
        live.contains(&sandbox_id),
        "recovered VM died within the grace period — the guest's \
         verify-init found no byte-identical /session-work.txt \
         (wrong disk, corrupted chunks, or mount failure)",
    );
    eprintln!("VERIFIED: guest alive at +{VERIFY_GRACE:?} — marker read back byte-identical");

    pooled
        .destroy(sandbox_id)
        .await
        .expect("destroy recovered sandbox");
    eprintln!("TEARDOWN: recovered VM destroyed + NBD daemon disconnected");
}

/// VM-free minimal repro for the teleport-canary disk corruption
/// (prod sessions 5fa742b7/4391e591): a write to a HIGH-offset chunk
/// through the kernel NBD device cannot be read back, while low
/// offsets work. No KVM, no Docker — synthetic 13-slot manifest
/// (9 populated, tail sparse), attach, pwrite at ~142 MiB, fsync,
/// pread back. Run as root (NBD ioctls).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Linux + modprobe nbd + root"]
async fn high_offset_write_reads_back_through_the_device() {
    use std::io::{Read, Seek, SeekFrom, Write};

    let nbd_path = PathBuf::from(
        std::env::var("ENGRAM_TEST_NBD_DEVICE").unwrap_or_else(|_| "/dev/nbd0".to_string()),
    );
    if !nbd_path.exists() {
        eprintln!("SKIP: {} not present", nbd_path.display());
        return;
    }
    let work = tempfile::tempdir().expect("work");
    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(work.path().join("blob")));
    let store = ChunkStore::new(blob);
    let mut cache_cfg = ChunkCacheConfig::new(work.path().join("cache"));
    cache_cfg.budget_bytes = 1024 * 1024 * 1024;
    let cache = ChunkCache::new(cache_cfg);

    // 13-slot device (217104384 bytes), 9 populated chunks — the same
    // shape the baked ext4 produced in the failing e2e.
    const CHUNK: u64 = 16 * 1024 * 1024;
    const TOTAL: u64 = 217_104_384;
    let mut chunks = Vec::new();
    for i in 0..9u64 {
        let body = vec![(i as u8).wrapping_add(1); CHUNK as usize];
        let hash = store.put_chunk(&body).await.expect("put chunk");
        chunks.push(engram_chunk_store::manifest::ChunkRef {
            offset: i * CHUNK,
            hash,
        });
    }
    let manifest = engram_chunk_store::Manifest {
        schema_version: engram_chunk_store::manifest::MANIFEST_SCHEMA_VERSION,
        kind: ManifestKind::Disk,
        chunk_size: engram_chunk_store::manifest::ChunkSize::bytes(CHUNK),
        total_bytes: TOTAL,
        chunks,
        parent: None,
        working_set_trace: None,
        annotations: serde_json::Value::Null,
    };
    let mref = ManifestRef::new();
    store
        .put_manifest(mref, &manifest)
        .await
        .expect("put manifest");

    let pool = NbdSlotAllocator::from_paths(vec![nbd_path.clone()]).expect("pool");
    let state = attach_manifest(
        mref,
        cache,
        Arc::new(store),
        &pool,
        u64::MAX,
        /*fork=*/ false,
    )
    .await
    .expect("attach");
    let dev = state.device_path().to_path_buf();
    eprintln!("attached at {}", dev.display());

    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&dev)
        .expect("open device");
    // The kernel applies NBD_SET_SIZE asynchronously w.r.t. open();
    // wait for the device to report its full size.
    for i in 0..40 {
        let size = f.seek(SeekFrom::End(0)).expect("size probe");
        if size == TOTAL {
            break;
        }
        eprintln!("device size {size} != {TOTAL} (attempt {i}); waiting");
        tokio::time::sleep(Duration::from_millis(100)).await;
        f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&dev)
            .expect("re-open device");
    }
    let final_size = f.seek(SeekFrom::End(0)).expect("size probe");
    eprintln!("device size: {final_size} (want {TOTAL})");

    // Control: low offset read.
    let mut low = vec![0u8; 4096];
    f.seek(SeekFrom::Start(0)).unwrap();
    f.read_exact(&mut low).expect("low-offset read");
    assert!(low.iter().all(|b| *b == 1), "chunk 0 content");

    // The probe: 8 MiB at 142,606,336 (inside populated chunk 8).
    let probe_off = 142_606_336u64;
    let payload: Vec<u8> = (0..8 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    f.seek(SeekFrom::Start(probe_off)).unwrap();
    f.write_all(&payload).expect("probe write");
    f.sync_data().expect("fsync");

    let mut back = vec![0u8; payload.len()];
    f.seek(SeekFrom::Start(probe_off)).unwrap();
    f.read_exact(&mut back).expect("probe read-back");
    assert_eq!(
        back, payload,
        "a high-offset write must read back byte-identical through the NBD device"
    );

    // Also exercise a SPARSE tail slot (chunk 10 — a manifest hole).
    let hole_off = 10 * CHUNK + 1024;
    f.seek(SeekFrom::Start(hole_off)).unwrap();
    f.write_all(&payload[..4096]).expect("hole write");
    f.sync_data().expect("fsync 2");
    let mut hole_back = vec![0u8; 4096];
    f.seek(SeekFrom::Start(hole_off)).unwrap();
    f.read_exact(&mut hole_back).expect("hole read-back");
    assert_eq!(
        hole_back,
        payload[..4096],
        "hole-backed chunk write survives"
    );
    drop(f);
    drop(state);
}
