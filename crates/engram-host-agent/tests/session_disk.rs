//! The session disk under a real Firecracker guest: a checkpoint
//! reproduces the guest's disk on another disk, the disk half of a
//! checkpoint stays bounded while the guest writes at full rate, and a
//! host-agent restart keeps a running guest and every write it makes.
//!
//! Over the dm-thin pool when `ENGRAM_TEST_THIN_POOL` names one (the CI
//! root lane), else over sparse files. Gating as `checkpoint_chain.rs`.
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]
#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use engram_chunk_store::ChunkStore;
use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::ids::SandboxId;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_core::types::snapshot::DiskRole;
use engram_host_agent::pooled_backend::PooledBackend;
use engram_host_core::disk::{Capacity, SessionDisk, Volume, VolumeName};
use engram_host_core::SessionVolumes;
use engram_rootfs_materializer::{InitInjection, Transport};
use engram_sandbox_firecracker::{
    FirecrackerBackend, FirecrackerConfig, RestoreMode, ENGRAM_AGENTD_PORT,
};
use futures::StreamExt;

/// The disk half of a checkpoint under full-rate writes: one page-cache
/// sync plus one thin snapshot, never a function of how much the guest
/// has written in all.
const DISK_PAUSE_BOUND: Duration = Duration::from_secs(2);

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker; bakes a rootfs and boots microVMs"]
async fn a_checkpoint_reproduces_the_guest_disk_on_another_disk() {
    let Some(rig) = Rig::gate().await else { return };
    let (pooled, _volumes) = rig.pooled(common::session_disk(rig.work.path()).await);
    let sandbox = SandboxId::new();
    pooled
        .create(sandbox, rig.spec().await)
        .await
        .expect("create");
    pooled.bind_session(engram_core::SessionId::new(), sandbox);
    let sum = exec(
        &pooled,
        sandbox,
        "head -c 4194304 /dev/urandom > /root/data && sync \
         && sha256sum /root/data | cut -d' ' -f1",
    )
    .await;
    let checkpoint = pooled
        .checkpoint_sandbox(sandbox)
        .await
        .expect("checkpoint");
    assert!(
        checkpoint.disk_manifest.is_some(),
        "the checkpoint publishes a disk manifest"
    );
    pooled.destroy(sandbox).await.expect("destroy");

    // A second host: another disk, the same chunk store.
    let other_disk: Arc<dyn SessionDisk> = Arc::new(
        engram_host_core::FileDisk::new(
            rig.work.path().join("other-host-volumes"),
            engram_chunk_store::DEFAULT_DISK_CHUNK_SIZE,
        )
        .expect("file disk"),
    );
    let (other, _) = rig.pooled(other_disk);
    let resumed = SandboxId::new();
    other
        .restore(resumed, checkpoint, None)
        .await
        .expect("restore on the other disk");
    let again = exec(&other, resumed, "sha256sum /root/data | cut -d' ' -f1").await;
    assert_eq!(
        again.trim(),
        sum.trim(),
        "the manifest reproduces the guest's bytes"
    );
    other.destroy(resumed).await.expect("destroy resumed");
}

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker; bakes a rootfs and boots microVMs"]
async fn the_disk_pause_is_bounded_while_the_guest_writes_at_full_rate() {
    let Some(rig) = Rig::gate().await else { return };
    let (pooled, volumes) = rig.pooled(common::session_disk(rig.work.path()).await);
    let sandbox = SandboxId::new();
    pooled
        .create(sandbox, rig.spec().await)
        .await
        .expect("create");
    pooled.bind_session(engram_core::SessionId::new(), sandbox);

    // 64 MiB per round, every round fsynced; `/root/rounds` counts them.
    assert_free_mib(&pooled, sandbox, 64 + 8).await;
    exec(
        &pooled,
        sandbox,
        &in_background(
            "i=0; while :; do \
               dd if=/dev/zero of=/root/fill bs=1M count=64 conv=notrunc,fsync 2>/dev/null; \
               i=$((i+1)); echo $i > /root/rounds; done",
        ),
    )
    .await;
    let written =
        common::poll_until_async(Duration::from_secs(120), Duration::from_millis(250), || {
            let pooled = pooled.clone();
            async move {
                exec(&pooled, sandbox, "cat /root/rounds 2>/dev/null || echo 0")
                    .await
                    .trim()
                    .parse::<u64>()
                    .unwrap_or(0)
                    >= 4
            }
        })
        .await;
    assert!(written, "the guest writes at full rate");

    for _ in 0..3 {
        let t = Instant::now();
        let pending = volumes
            .take(sandbox, DiskRole::Root)
            .await
            .expect("snapshot the live disk");
        let pause = t.elapsed();
        volumes.discard(pending).await.expect("discard");
        eprintln!(
            "DISK: sync + snapshot under full-rate writes {} ms",
            pause.as_millis()
        );
        assert!(
            pause <= DISK_PAUSE_BOUND,
            "sync + snapshot took {pause:?}, over {DISK_PAUSE_BOUND:?}"
        );
    }
    // The whole checkpoint still lands under the writes.
    let checkpoint = pooled
        .checkpoint_sandbox(sandbox)
        .await
        .expect("checkpoint under full-rate writes");
    assert!(checkpoint.disk_manifest.is_some());
    pooled.destroy(sandbox).await.expect("destroy");
}

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker; bakes a rootfs and boots microVMs"]
async fn a_host_agent_restart_keeps_the_guest_and_every_write() {
    let Some(rig) = Rig::gate().await else { return };
    let counted = Arc::new(CountingDisk {
        inner: common::session_disk(rig.work.path()).await,
        writes: AtomicU64::new(0),
    });
    let (generation_a, volumes) = rig.pooled(counted.clone());
    let sandbox = SandboxId::new();
    generation_a
        .create(sandbox, rig.spec().await)
        .await
        .expect("create");
    // Every line is the next integer, fsynced, so a lost write is a gap.
    // The writer stops at a stop file and reports when its last line is in.
    exec(
        &generation_a,
        sandbox,
        &in_background(
            "i=0; while [ ! -e /root/stop ]; do \
               i=$((i+1)); echo $i >> /root/seq; sync; done; \
             echo done > /root/writer.done",
        ),
    )
    .await;
    let before_roll =
        common::poll_until_async(Duration::from_secs(60), Duration::from_millis(250), || {
            let pooled = generation_a.clone();
            async move { lines(&pooled, sandbox).await >= 50 }
        })
        .await;
    assert!(before_roll, "the guest writes before the roll");
    let disk_writes_before = counted.writes.load(Ordering::Relaxed);

    // The roll: generation A dies with no destructors; generation B
    // reattaches the running guest over the same disk, with its own view
    // of the volumes.
    let _generation_a_dead = generation_a;
    drop(volumes);
    let inner_b = Arc::new(FirecrackerBackend::new(rig.work.path(), rig.cfg.clone()));
    let report = engram_host_agent::live_attach::reattach_pass(rig.work.path(), &inner_b)
        .await
        .expect("reattach pass");
    assert!(
        report.reattached.iter().any(|o| matches!(
            o,
            engram_host_agent::live_attach::ReattachOutcome::Reattached { sandbox_id, .. }
                if *sandbox_id == sandbox.to_string()
        )),
        "generation B reattaches the running guest",
    );
    let generation_b = Arc::new(
        PooledBackend::new(inner_b as Arc<dyn SandboxBackend>)
            .with_chunk_store(rig.chunk_store.clone())
            .with_session_volumes(Arc::new(SessionVolumes::new(
                counted.clone(),
                engram_fill::FillEngine::new(
                    rig.chunk_store.clone(),
                    engram_fill::Config::default(),
                ),
            )))
            .with_checkpoint_dir(rig.work.path().join("checkpoints")),
    );
    generation_b.set_self_ref(&generation_b);
    generation_b.reconcile_volumes().await;
    assert_eq!(
        counted.writes.load(Ordering::Relaxed),
        disk_writes_before,
        "the roll changes nothing on the disk"
    );

    let at_roll = lines(&generation_b, sandbox).await;
    let after_roll =
        common::poll_until_async(Duration::from_secs(60), Duration::from_millis(250), || {
            let pooled = generation_b.clone();
            async move { lines(&pooled, sandbox).await >= at_roll + 50 }
        })
        .await;
    assert!(after_roll, "the guest keeps writing through the roll");
    exec(
        &generation_b,
        sandbox,
        "touch /root/stop; while [ ! -e /root/writer.done ]; do sleep 0.1; done; \
         sync && awk 'NR != $1 { exit 1 }' /root/seq",
    )
    .await;
    generation_b.destroy(sandbox).await.expect("destroy");
}

/// A guest command that runs `script` detached from the exec. The guest's
/// busybox has no `nohup`; agentd's exec has no controlling tty, so none is
/// needed. The shell's own stdio goes to /dev/null first: busybox ash leaks
/// the fds it saves for a per-job redirection into a backgrounded child,
/// which would hold the exec's pipes open.
fn in_background(script: &str) -> String {
    format!("exec </dev/null >/dev/null 2>&1; sh -c '{script}' &")
}

/// Fails the test, with the number, when the guest's root filesystem has
/// less than `need_mib` free: a writer loop that hits a full disk stops
/// counting its rounds and reads as never having started.
async fn assert_free_mib(pooled: &Arc<PooledBackend>, sandbox: SandboxId, need_mib: u64) {
    let free_kib: u64 = exec(pooled, sandbox, "df -k /root | awk 'NR==2 {print $4}'")
        .await
        .trim()
        .parse()
        .expect("df prints the free KiB of /root");
    assert!(
        free_kib / 1024 >= need_mib,
        "the guest root has {} MiB free; the test writes {need_mib} MiB",
        free_kib / 1024
    );
}

async fn lines(pooled: &Arc<PooledBackend>, sandbox: SandboxId) -> u64 {
    exec(pooled, sandbox, "wc -l < /root/seq 2>/dev/null || echo 0")
        .await
        .trim()
        .parse()
        .unwrap_or(0)
}

/// Counts the calls that change the disk.
struct CountingDisk {
    inner: Arc<dyn SessionDisk>,
    writes: AtomicU64,
}

#[async_trait]
impl SessionDisk for CountingDisk {
    async fn create(&self, name: &VolumeName, size_bytes: u64) -> std::io::Result<Volume> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.inner.create(name, size_bytes).await
    }
    async fn snapshot(&self, origin: &Volume, name: &VolumeName) -> std::io::Result<Volume> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.inner.snapshot(origin, name).await
    }
    async fn open(&self, name: &VolumeName) -> std::io::Result<Volume> {
        self.inner.open(name).await
    }
    async fn delete(&self, name: &VolumeName) -> std::io::Result<()> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.inner.delete(name).await
    }
    async fn list(&self) -> std::io::Result<Vec<VolumeName>> {
        self.inner.list().await
    }
    async fn rename(&self, from: &VolumeName, to: &VolumeName) -> std::io::Result<()> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.inner.rename(from, to).await
    }
    async fn capacity(&self) -> std::io::Result<Capacity> {
        self.inner.capacity().await
    }
    async fn dirty_blocks(
        &self,
        before: &Volume,
        after: &Volume,
        block_bytes: u64,
    ) -> std::io::Result<Vec<u64>> {
        self.inner.dirty_blocks(before, after, block_bytes).await
    }
}

struct Rig {
    work: tempfile::TempDir,
    _images: tempfile::TempDir,
    chunk_store: ChunkStore,
    cfg: FirecrackerConfig,
    agentd_slot: engram_core::types::sandbox::AuxRoDrive,
    disk_manifest: engram_chunk_store::ManifestRef,
}

impl Rig {
    /// `None`, with a `SKIP:` line, when the host cannot boot a guest.
    async fn gate() -> Option<Self> {
        let kernel = match std::env::var("FC_TEST_KERNEL") {
            Ok(p) => PathBuf::from(p),
            Err(_) => {
                eprintln!("SKIP: FC_TEST_KERNEL not set");
                return None;
            }
        };
        if !Path::new("/dev/kvm").exists() {
            eprintln!("SKIP: /dev/kvm not present");
            return None;
        }
        for bin in ["firecracker", "mksquashfs"] {
            if std::env::var_os("PATH")
                .map(|p| !std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
                .unwrap_or(true)
            {
                eprintln!("SKIP: {bin} not on PATH");
                return None;
            }
        }
        let busybox = common::find_busybox()?;
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
        let agent = Path::new(&manifest_dir)
            .join("../../target/x86_64-unknown-linux-musl/release/engram-agentd");
        if !agent.exists() {
            eprintln!("SKIP: musl engram-agentd not built at {}", agent.display());
            return None;
        }

        let images = tempfile::tempdir().expect("images dir");
        let work = tempfile::tempdir().expect("work dir");
        let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
            engram_storage_local::LocalBlobStorage::new(work.path().join("blob")),
        );
        let mut cache_cfg =
            engram_chunk_store::cache::ChunkCacheConfig::new(work.path().join("chunk-cache"));
        cache_cfg.budget_bytes = 1024 * 1024 * 1024;
        let chunk_store =
            ChunkStore::new(blob).with_chunk_cache(engram_chunk_store::ChunkCache::new(cache_cfg));
        let baked = common::bake_fixture_ext4(
            &images.path().join("rootfs.ext4"),
            &chunk_store,
            &busybox,
            Some(InitInjection {
                vsock_port: ENGRAM_AGENTD_PORT,
                transport: Transport::Vsock,
                init_script: None,
            }),
            |_tree| Ok(()),
        )
        .await;
        let mut cfg = FirecrackerConfig::with_kernel(kernel);
        let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agent);
        cfg.bundle_dir = staged.bundle_dir.clone();
        cfg.net_pool = None;
        cfg.restore_mode = RestoreMode::File;
        cfg.track_dirty_pages = true;
        cfg.default_boot_args =
            "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
        std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");
        Some(Self {
            work,
            _images: images,
            chunk_store,
            cfg,
            agentd_slot: staged.agentd_slot(),
            disk_manifest: baked.disk_manifest,
        })
    }

    /// A backend over `disk`, with the volumes it owns.
    fn pooled(&self, disk: Arc<dyn SessionDisk>) -> (Arc<PooledBackend>, Arc<SessionVolumes>) {
        let volumes = Arc::new(SessionVolumes::new(
            disk,
            engram_fill::FillEngine::new(self.chunk_store.clone(), engram_fill::Config::default()),
        ));
        let inner = Arc::new(FirecrackerBackend::new(self.work.path(), self.cfg.clone()));
        let pooled = Arc::new(
            PooledBackend::new(inner)
                .with_chunk_store(self.chunk_store.clone())
                .with_session_volumes(volumes.clone())
                .with_checkpoint_dir(self.work.path().join("checkpoints")),
        );
        pooled.set_self_ref(&pooled);
        (pooled, volumes)
    }

    async fn spec(&self) -> SandboxSpec {
        SandboxSpec {
            image: "engram-session-disk-test".into(),
            rootfs_source: None,
            image_uri: None,
            rootfs_manifest: Some(
                common::fork_lineage(&self.chunk_store, self.disk_manifest).await,
            ),
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 256 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: HashMap::new(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: vec![self.agentd_slot.clone()],
            swap_mib: None,
            swap_source: None,
            swap_manifest: None,
        }
    }
}

async fn exec(backend: &Arc<PooledBackend>, id: SandboxId, cmd: &str) -> String {
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
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => panic!("agent never came up: {e:?}"),
        }
    };
    let mut events = stream.events;
    let (mut stdout, mut stderr, mut exit) = (Vec::new(), Vec::new(), None);
    while let Some(ev) = events.next().await {
        use engram_core::types::sandbox::ExecEvent;
        match ev {
            ExecEvent::Stdout(b) => stdout.extend_from_slice(&b),
            ExecEvent::Stderr(b) => stderr.extend_from_slice(&b),
            ExecEvent::Exit(code) => {
                exit = code;
                break;
            }
            ExecEvent::Refused(reason) => panic!("exec refused: {reason}"),
        }
    }
    assert_eq!(
        exit,
        Some(0),
        "guest cmd failed: {cmd}\nstderr: {}",
        String::from_utf8_lossy(&stderr),
    );
    String::from_utf8_lossy(&stdout).into_owned()
}
