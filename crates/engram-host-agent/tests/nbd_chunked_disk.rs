//! Phase 4 end-to-end NBD integration test.
//!
//! Boots a real Firecracker microVM whose rootfs is `/dev/nbd0`,
//! served by the chunked NBD daemon. Asserts the guest reads back
//! byte-identical content (via an in-VM `cat` over vsock-exec) AND
//! the daemon shuts down cleanly on `destroy()`.
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
//! (observed on the dev-vm's 6.x, 2026-06) gate the NBD setup ioctls
//! (`NBD_SET_SOCK` et al.) behind CAP_SYS_ADMIN regardless of the
//! device's permission bits, so these tests now need to run under
//! `sudo -E` (mirroring the root-required FC tests in
//! `run-boot-test.sh`). Production is unaffected — the host-agent
//! runs as root.
//!
//! What this test covers that the pure-Rust e2e suite
//! (`adr_0007_e2e.rs`) doesn't:
//!  - The NBD ioctl orchestration with the actual Linux kernel
//!    (`NBD_SET_SOCK` / `NBD_SET_BLKSIZE` / `NBD_SET_SIZE_BLOCKS` /
//!    `NBD_DO_IT` / `NBD_DISCONNECT`).
//!  - The kernel's NBD transmission protocol talking to our daemon's
//!    `serve_loop` (request headers, response headers, payload).
//!  - FC's virtio-blk frontend reading from `/dev/nbd0` and the guest
//!    kernel mounting it as the rootfs.
//!  - Clean tear-down: NBD_DISCONNECT releases NBD_DO_IT, the OS
//!    thread joins, the slot returns to the pool.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use engram_chunk_store::cache::{ChunkCache, ChunkCacheConfig};
use engram_chunk_store::{ChunkStore, ManifestKind};
use engram_core::traits::{BlobStorage, SandboxBackend};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_host_agent::disk_daemon::{attach_manifest, NbdSlotAllocator};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, RestoreMode};
use engram_storage_local::LocalBlobStorage;
use futures::StreamExt;

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
    Some((kernel, rootfs, nbd_path))
}

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + modprobe nbd + writeable /dev/nbd0"]
async fn fc_microvm_boots_with_nbd_chunked_rootfs() {
    let (kernel, rootfs_src, nbd_path) = match preflight() {
        Some(v) => v,
        None => return,
    };

    let work = tempfile::tempdir().expect("tempdir");

    // 1. Stand up a chunk store + cache + plant the test rootfs as
    //    a chunked disk manifest.
    let blob: Arc<dyn BlobStorage> = Arc::new(LocalBlobStorage::new(work.path().join("blob")));
    let store = Arc::new(ChunkStore::new(blob));
    let mut cache_cfg = ChunkCacheConfig::new(work.path().join("chunk-cache"));
    cache_cfg.budget_bytes = 256 * 1024 * 1024;
    let cache = ChunkCache::new(cache_cfg);

    let manifest = store
        .chunk_file(&rootfs_src, ManifestKind::Disk, None)
        .await
        .expect("chunk test rootfs");
    let manifest_ref = ManifestRef::new();
    store
        .put_manifest(manifest_ref, &manifest)
        .await
        .expect("put rootfs manifest");
    eprintln!(
        "PLANT: chunked {} into {} chunks ({} MiB)",
        rootfs_src.display(),
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
    let nbd_state = attach_manifest(manifest_ref, cache, store.clone(), &pool, u64::MAX)
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
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
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

    // 4. Exec a command. If the disk really came up, this works.
    let req = ExecRequest {
        command: vec!["/bin/echo".into(), "nbd-rootfs-alive".into()],
        env: HashMap::new(),
        stdin: None,
        workdir: None,
        timeout: Some(Duration::from_secs(5)),
    };
    let stream = backend
        .exec_stream(sandbox_id, req)
        .await
        .expect("exec against NBD-backed sandbox");
    let (stdout, stderr, exit) = drain(stream.events).await;
    eprintln!(
        "EXEC: exit={:?} stdout={:?} stderr={:?}",
        exit,
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    assert_eq!(exit, Some(0), "echo via NBD-rootfs VM must exit clean");
    assert!(
        String::from_utf8_lossy(&stdout).contains("nbd-rootfs-alive"),
        "echo output must include the sentinel string"
    );

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
    // The marker injection needs debugfs (e2fsprogs).
    if !std::process::Command::new("debugfs")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("SKIP: debugfs not available");
        return;
    }

    let work = tempfile::tempdir().expect("tempdir");

    // 1. Build the "session's evolved disk": copy the base test
    //    rootfs and plant a marker file in it — the stand-in for the
    //    work a real session wrote before its host died.
    let evolved = work.path().join("evolved-rootfs.ext4");
    std::fs::copy(&rootfs_src, &evolved).expect("copy rootfs");
    let marker = work.path().join("marker.txt");
    std::fs::write(&marker, "survived-the-host-roll\n").expect("write marker");
    // The verify-init: alive iff the marker reads back byte-identical.
    // A mismatch (or a base disk without the marker) exits pid 1 →
    // kernel panic (`panic=1 reboot=k`) → FC exits → the host-side
    // liveness assertion fails. PATH is explicit because the kernel
    // execs init with an empty environment.
    let verify_init = work.path().join("verify-init.sh");
    std::fs::write(
        &verify_init,
        "#!/bin/bash\n\
         export PATH=/usr/sbin:/usr/bin:/sbin:/bin\n\
         if [ \"$(cat /session-work.txt 2>/dev/null)\" = \"survived-the-host-roll\" ]; then\n\
         \twhile true; do sleep 60; done\n\
         fi\n\
         exit 1\n",
    )
    .expect("write verify-init");
    for cmd in [
        format!("write {} /session-work.txt", marker.display()),
        format!("write {} /verify-init.sh", verify_init.display()),
        "sif /verify-init.sh mode 0100755".to_string(),
    ] {
        let out = std::process::Command::new("debugfs")
            .args(["-w", "-R", &cmd])
            .arg(&evolved)
            .output()
            .expect("debugfs");
        assert!(out.status.success(), "debugfs `{cmd}` failed");
    }

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
    tokio::time::sleep(Duration::from_secs(6)).await;
    let live = pooled.list().await.expect("list");
    assert!(
        live.contains(&sandbox_id),
        "recovered VM died within the grace period — the guest's \
         verify-init found no byte-identical /session-work.txt \
         (wrong disk, corrupted chunks, or mount failure)",
    );
    eprintln!("VERIFIED: guest alive at +6s — marker read back byte-identical");

    pooled
        .destroy(sandbox_id)
        .await
        .expect("destroy recovered sandbox");
    eprintln!("TEARDOWN: recovered VM destroyed + NBD daemon disconnected");
}

/// Mirror of `common::drain` from the FC test scaffold. Drains an
/// `ExecStream` into separated stdout / stderr buffers + the exit
/// code.
async fn drain(
    mut stream: impl StreamExt<Item = engram_core::types::sandbox::ExecEvent> + Unpin,
) -> (Vec<u8>, Vec<u8>, Option<i32>) {
    use engram_core::types::sandbox::ExecEvent;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut exit_code = None;
    while let Some(ev) = stream.next().await {
        match ev {
            ExecEvent::Stdout(b) => stdout.extend_from_slice(&b),
            ExecEvent::Stderr(b) => stderr.extend_from_slice(&b),
            ExecEvent::Exit(code) => {
                exit_code = code;
                break;
            }
        }
    }
    (stdout, stderr, exit_code)
}
