//! ADR 0045 C2 (2026-10-07 addendum): a File-mode FRESH create is a
//! valid post-copy source.
//!
//! Production restores every fresh create on `RestoreMode::File` (a
//! private mapping of the per-image memfile, ADR 0092). The first fleet
//! run of the ADR 0123 teleport machine aborted every live move with
//! "FC process maps no substrate base file" because the source check
//! keyed on the substrate base DIR, and it ran inside the blackout.
//!
//! This test pins the fix end to end on real KVM, sized to the property:
//! the restore records the memfile as the guest's memory backing;
//! `migration_presetup` admits the VM pause-free; the post-copy capture
//! finds the guest VMAs by that path and seals a non-empty dirty map
//! (the guest wrote a 4 MiB sentinel into RAM after the restore, so the
//! pagemap scan MUST classify those anonymous COW pages as sealed); and
//! `migration_abort` resumes the guest with the sentinel intact.
//!
//! Runs in the NBD CI lane (root, `ENGRAM_TEST_NBD_DEVICE`): presetup
//! requires the chunked NBD rootfs (the disk half of post-copy), and the
//! vmstate-only capture needs the forked Firecracker (`ENGRAM_FC_FORK_BIN`).
#![allow(clippy::disallowed_methods)]
#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_host_agent::disk_daemon::NbdSlotAllocator;
use engram_host_agent::migrate_peer::PeerServer;
use engram_host_agent::pooled_backend::PooledBackend;
use engram_rootfs_materializer::{InitInjection, Transport};
use engram_sandbox_firecracker::{
    FirecrackerBackend, FirecrackerConfig, RestoreMode, ENGRAM_AGENTD_PORT,
};
use futures::StreamExt;

#[tokio::test]
#[ignore = "requires Linux + KVM + ENGRAM_FC_FORK_BIN + /dev/nbd0 (root) + Docker; boots microVMs"]
async fn file_mode_fresh_create_is_a_post_copy_source() {
    let Some(env) = TestEnv::gate() else { return };
    let pooled = env.pooled();
    // The page server is a presetup precondition; the capture only
    // registers the export with it (no dest dials in this test).
    pooled.set_migrate_peer_server(PeerServer::new(0, None, Some(env.chunk_store.clone())));
    let rootfs = env.bake("engram-postcopy-fresh-source").await;

    let (progress_tx, _progress_rx) = tokio::sync::mpsc::channel(16);
    let result = pooled
        .build_base_snapshot(
            engram_core::traits::sandbox::BuildBaseSnapshotRequest {
                spec: env.spec(&rootfs),
                warm: None,
                capture_env: Default::default(),
                capture_egress: None,
                cold_base_plan: engram_core::types::capture_job::ColdBasePlan::NotApplicable,
            },
            progress_tx,
        )
        .await
        .expect("base-snapshot capture");

    // A fresh create: File mode by construction (ADR 0092) — the guest's
    // RAM is a private mapping of the base snapshot's memfile, and the
    // chain is seeded at create from the base manifest.
    let vm = pooled
        .restore_fresh(result.snapshot, Vec::new())
        .await
        .expect("fresh create from the base snapshot");
    let view = pooled
        .post_copy_source_view(vm)
        .expect("a File-mode fresh create records its memory backing");
    assert!(
        view.memory_backing.ends_with("memory.bin"),
        "fresh create backing is the per-image memfile, got {}",
        view.memory_backing.display()
    );

    // Dirty guest RAM after the restore so the seal has something to say.
    let sentinel = exec(
        &pooled,
        vm,
        "head -c 4194304 /dev/urandom > /dev/shm/sentinel \
         && sha256sum /dev/shm/sentinel | cut -d' ' -f1",
    )
    .await
    .trim()
    .to_string();
    assert_eq!(sentinel.len(), 64, "sha helper sanity: {sentinel}");

    let presetup = pooled
        .migration_presetup(vm)
        .await
        .expect("presetup admits a File-mode fresh create (pause-free)");
    let out = pooled
        .migration_capture_postcopy(vm, &presetup.export_id)
        .await
        .expect("post-copy capture finds the guest VMAs by the recorded backing");
    assert!(out.total_chunks > 0, "chain geometry: {out:?}");
    assert!(
        out.sealed_chunks > 0,
        "the 4 MiB sentinel dirtied anonymous COW pages the pagemap scan must seal: {out:?}"
    );
    assert!(
        out.sealed_chunks < out.total_chunks,
        "clean file-backed pages must NOT be sealed (the whole image sealed = scan broken): {out:?}"
    );

    // Abort = resume in place. The guest continues with its RAM intact.
    pooled
        .migration_abort(vm, &presetup.export_id)
        .await
        .expect("abort resumes the paused source");
    let after = exec(&pooled, vm, "sha256sum /dev/shm/sentinel | cut -d' ' -f1").await;
    assert_eq!(after.trim(), sentinel, "sentinel survived capture + abort");

    pooled.destroy(vm).await.expect("destroy");
}

struct TestEnv {
    kernel: std::path::PathBuf,
    fork_bin: std::path::PathBuf,
    nbd_device: std::path::PathBuf,
    staged: common::StagedAgentdBundle,
    busybox: std::path::PathBuf,
    work: tempfile::TempDir,
    images: tempfile::TempDir,
    chunk_store: engram_chunk_store::ChunkStore,
}

impl TestEnv {
    fn gate() -> Option<Self> {
        let kernel = match std::env::var("FC_TEST_KERNEL") {
            Ok(p) => std::path::PathBuf::from(p),
            Err(_) => {
                eprintln!("SKIP: FC_TEST_KERNEL not set");
                return None;
            }
        };
        if !Path::new("/dev/kvm").exists() {
            eprintln!("SKIP: /dev/kvm not present");
            return None;
        }
        let fork_bin = match std::env::var("ENGRAM_FC_FORK_BIN") {
            Ok(p) if !p.is_empty() => std::path::PathBuf::from(p),
            _ => {
                eprintln!(
                    "SKIP: ENGRAM_FC_FORK_BIN not set (the vmstate-only capture is fork-only)"
                );
                return None;
            }
        };
        let nbd_device = std::path::PathBuf::from(
            std::env::var("ENGRAM_TEST_NBD_DEVICE").unwrap_or_else(|_| "/dev/nbd0".to_string()),
        );
        if std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&nbd_device)
            .is_err()
        {
            eprintln!(
                "SKIP: cannot open {} R/W (needs `modprobe nbd` and root)",
                nbd_device.display()
            );
            return None;
        }
        let mksquashfs_missing = std::env::var_os("PATH")
            .map(|p| !std::env::split_paths(&p).any(|d| d.join("mksquashfs").is_file()))
            .unwrap_or(true);
        if mksquashfs_missing {
            eprintln!("SKIP: mksquashfs not on PATH");
            return None;
        }
        let Some(busybox) = common::find_busybox() else {
            eprintln!("SKIP: no static busybox (apt install busybox-static or set BUSYBOX_STATIC)");
            return None;
        };
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let agent = Path::new(&manifest_dir)
            .join("../../target/x86_64-unknown-linux-musl/release/engram-agentd");
        if !agent.exists() {
            eprintln!(
                "SKIP: musl engram-agentd not built at {} — \
                 cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release",
                agent.display(),
            );
            return None;
        }
        let work = tempfile::tempdir().expect("work dir");
        let images = tempfile::tempdir().expect("images dir");
        let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agent);
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(work.path().join("blob")),
        );
        let chunk_store = engram_chunk_store::ChunkStore::new(blob);
        std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");
        Some(Self {
            kernel,
            fork_bin,
            nbd_device,
            staged,
            busybox,
            work,
            images,
            chunk_store,
        })
    }

    fn pooled(&self) -> Arc<PooledBackend> {
        let mut cfg = FirecrackerConfig::with_kernel(self.kernel.clone());
        cfg.firecracker_bin = self.fork_bin.clone();
        cfg.bundle_dir = self.staged.bundle_dir.clone();
        cfg.net_pool = None;
        cfg.restore_mode = RestoreMode::File;
        cfg.track_dirty_pages = true;
        cfg.default_boot_args =
            "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
        let inner = Arc::new(FirecrackerBackend::new(self.work.path(), cfg));
        let mut cache_cfg =
            engram_chunk_store::cache::ChunkCacheConfig::new(self.work.path().join("chunk-cache"));
        cache_cfg.budget_bytes = 1024 * 1024 * 1024;
        Arc::new(
            PooledBackend::new(inner)
                .with_chunk_store(
                    self.chunk_store.clone(),
                    self.work.path().join("materialize"),
                )
                .with_chunk_cache(engram_chunk_store::ChunkCache::new(cache_cfg))
                .with_checkpoint_dir(self.work.path().join("checkpoints"))
                .with_nbd_pool(
                    NbdSlotAllocator::from_paths(vec![self.nbd_device.clone()])
                        .expect("nbd slot pool"),
                ),
        )
    }

    async fn bake(&self, name: &str) -> std::path::PathBuf {
        let outcome = common::bake_fixture_ext4(
            &self.images.path().join(format!("{name}.ext4")),
            &self.chunk_store,
            &self.busybox,
            Some(InitInjection {
                vsock_port: ENGRAM_AGENTD_PORT,
                transport: Transport::Vsock,
                init_script: None,
            }),
            |_tree| Ok(()),
        )
        .await;
        outcome.rootfs_path
    }

    fn spec(&self, rootfs: &Path) -> SandboxSpec {
        SandboxSpec {
            image: "engram-postcopy-fresh-source".into(),
            rootfs_source: Some(rootfs.to_path_buf()),
            image_uri: None,
            rootfs_manifest: None,
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 256 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: HashMap::new(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: vec![self.staged.agentd_slot()],
            swap_mib: None,
        }
    }
}

async fn exec(backend: &Arc<PooledBackend>, id: engram_core::SandboxId, cmd: &str) -> String {
    let req = ExecRequest {
        command: vec!["/bin/sh".into(), "-c".into(), cmd.into()],
        stdin: None,
        env: HashMap::new(),
        workdir: None,
        timeout: Some(Duration::from_secs(30)),
        exec_id: None,
        stdout_offset: None,
        stderr_offset: None,
        wake: None,
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let stream = loop {
        match backend.exec_stream(id, req.clone()).await {
            Ok(s) => break s,
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => panic!("agent never came up: {e:?}"),
        }
    };
    let mut events = stream.events;
    let mut stdout = Vec::new();
    while let Some(ev) = events.next().await {
        use engram_core::types::sandbox::ExecEvent;
        match ev {
            ExecEvent::Stdout(b) => stdout.extend_from_slice(&b),
            ExecEvent::Stderr(_) => {}
            ExecEvent::Exit(_) => break,
            ExecEvent::Refused(reason) => panic!("exec refused: {reason}"),
        }
    }
    String::from_utf8_lossy(&stdout).into_owned()
}
