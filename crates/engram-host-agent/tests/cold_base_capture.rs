//! ADR 0081 §B (P3): cold-base / warm-overlay capture against a REAL
//! Firecracker microVM.
//!
//! One test, sized to the property it proves (repo convention: prove
//! correctness with the least data/time that still demonstrates it —
//! never scale up for a throughput measurement in a CI test):
//!
//!   1. **Miss**: cold-boot, run a `[warm]` hook that dirties a handful
//!      of guest-RAM pages, and mint a cold base (the pre-hook Full
//!      capture) + a Diff overlay. Assert the overlay upload touches
//!      FAR fewer NEW chunk blobs than the cold base's own Full capture
//!      did — the overlay's memory manifest still describes the
//!      complete guest image (chunk COUNT is unchanged, ADR §B2: "no
//!      chain replay"), but almost every one of its chunk hashes is
//!      byte-identical to a chunk the cold base already wrote, so the
//!      write-through chunk store dedupes the upload down to just the
//!      hook-dirtied pages.
//!   2. **Hit**: a SECOND capture reusing the SAME cold base
//!      (`ColdBasePlan::Hit` built from the first capture's own
//!      reported identity) never cold-boots — `create()` is never
//!      called for it (proved indirectly: the Hit leg's wall-clock is
//!      far shorter than the cold-boot+Full Miss leg, which pays a
//!      real kernel boot + agentd handshake the restore never does),
//!      and its own result reports `freshly_captured: false` (no
//!      second `cold_bases` row minted).
//!
//! Linux + KVM + firecracker + Docker + mke2fs + musl agentd only (same
//! gating as `warm_hook_capture.rs`, whose `TestEnv` harness this
//! mirrors). Run on the dev-vm:
//!
//! ```sh
//! cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-host-agent --test cold_base_capture -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use engram_chunk_store::ChunkStore;
use engram_core::traits::sandbox::{BuildBaseSnapshotRequest, SandboxBackend};
use engram_core::traits::{BlobStorage, ByteStream};
use engram_core::types::capture_job::ColdBasePlan;
use engram_core::types::image::WarmConfig;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, SandboxSpec};
use engram_host_agent::pooled_backend::PooledBackend;
use engram_rootfs_materializer::{InitInjection, Transport};
use engram_sandbox_firecracker::{
    FirecrackerBackend, FirecrackerConfig, RestoreMode, ENGRAM_AGENTD_PORT,
};

/// Wraps a real `BlobStorage` and counts `put_streaming` calls — the
/// chunk store's write path calls this once per chunk it decides is
/// NEW (its dedup check is upstream of this call), so the count is a
/// direct proxy for "how much upload work did this capture cost."
struct CountingBlobStorage {
    inner: Arc<dyn BlobStorage>,
    puts: Arc<AtomicUsize>,
}

#[async_trait]
impl BlobStorage for CountingBlobStorage {
    async fn put_streaming(
        &self,
        key: &str,
        body: ByteStream,
    ) -> Result<u64, engram_core::error::BlobError> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.inner.put_streaming(key, body).await
    }
    async fn get_streaming(&self, key: &str) -> Result<ByteStream, engram_core::error::BlobError> {
        self.inner.get_streaming(key).await
    }
    async fn head(
        &self,
        key: &str,
    ) -> Result<engram_core::traits::BlobObjectMeta, engram_core::error::BlobError> {
        self.inner.head(key).await
    }
    async fn delete(&self, key: &str) -> Result<(), engram_core::error::BlobError> {
        self.inner.delete(key).await
    }
    async fn list_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<String>, engram_core::error::BlobError> {
        self.inner.list_prefix(prefix).await
    }
}

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; bakes a rootfs and boots microVMs"]
async fn cold_base_hit_skips_the_second_cold_boot_and_dedupes_the_overlay() {
    let Some(env) = TestEnv::gate() else { return };
    let puts = Arc::new(AtomicUsize::new(0));
    let pooled = env.pooled(puts.clone());
    let rootfs = env.bake("engram-cold-base-test").await;

    // A tiny hook: write one small marker file (a handful of dirtied
    // guest-RAM pages) and exit. Big enough to prove the diff overlay
    // is non-trivially exercised; small enough to keep the CI VM's
    // capture legs fast (this test measures RELATIVE cost between the
    // two legs, not absolute throughput).
    let warm = WarmConfig {
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            "dd if=/dev/zero of=/dev/shm/engram-cold-base-marker bs=4096 count=8 2>/dev/null"
                .into(),
        ],
        timeout_secs: Some(60),
        workdir: None,
        env: Vec::new(),
        network: None,
    };

    let content_key = "fc-test-cold-base-key".to_string();

    // ---- leg 1: Miss — cold-boot, mint the cold base, then the
    // overlay. ----
    let (tx1, _rx1) = tokio::sync::mpsc::channel(16);
    let miss_puts_before = puts.load(Ordering::SeqCst);
    let t0 = Instant::now();
    let result1 = pooled
        .build_base_snapshot(
            BuildBaseSnapshotRequest {
                spec: env.spec(&rootfs),
                warm: Some(warm.clone()),
                capture_env: Default::default(),
                capture_egress: None,
                cold_base_plan: ColdBasePlan::Miss {
                    content_key: content_key.clone(),
                    reason: engram_core::types::capture_job::ColdBaseMissReason::NoCandidate,
                },
            },
            tx1,
        )
        .await
        .expect("miss (cold-boot) capture must succeed");
    let miss_elapsed = t0.elapsed();
    let miss_puts = puts.load(Ordering::SeqCst) - miss_puts_before;
    let cold_base = result1
        .cold_base
        .clone()
        .expect("a Miss must report a minted cold base");
    assert!(
        cold_base.freshly_captured,
        "the Miss leg must mint a FRESH cold base"
    );
    assert_ne!(
        cold_base.snapshot.id, result1.snapshot.id,
        "the pre-hook cold base and the post-hook overlay must be distinct captures"
    );
    eprintln!(
        "SPIKE: miss leg elapsed={miss_elapsed:?} chunk_puts={miss_puts} \
         cold_base_id={} overlay_id={}",
        cold_base.snapshot.id, result1.snapshot.id
    );

    // ---- leg 2: Hit — restore the SAME cold base (as a claim handler
    // would resolve it from `cold_bases`), never cold-boot. ----
    let (tx2, _rx2) = tokio::sync::mpsc::channel(16);
    let hit_puts_before = puts.load(Ordering::SeqCst);
    let t1 = Instant::now();
    let result2 = pooled
        .build_base_snapshot(
            BuildBaseSnapshotRequest {
                spec: env.spec(&rootfs),
                warm: Some(warm),
                capture_env: Default::default(),
                capture_egress: None,
                cold_base_plan: ColdBasePlan::Hit {
                    content_key: content_key.clone(),
                    snapshot: Box::new(cold_base.snapshot.clone()),
                },
            },
            tx2,
        )
        .await
        .expect("hit (restore) capture must succeed");
    let hit_elapsed = t1.elapsed();
    let hit_puts = puts.load(Ordering::SeqCst) - hit_puts_before;
    let cb2 = result2
        .cold_base
        .expect("a Hit must still report the (unchanged) cold-base identity");
    assert!(
        !cb2.freshly_captured,
        "a Hit must NOT mint a second cold base"
    );
    assert_eq!(
        cb2.snapshot.id, cold_base.snapshot.id,
        "a Hit echoes the EXISTING candidate's identity, not a fresh one"
    );
    eprintln!("SPIKE: hit leg elapsed={hit_elapsed:?} chunk_puts={hit_puts}");

    // (b) no second cold boot: the Hit leg (restore + cheap diff) must
    // be substantially faster than the Miss leg (cold-boot + agentd
    // handshake + a Full memory capture) — the only two structural
    // differences between the legs are exactly the cold-boot cost and
    // the Full-vs-Diff memory capture cost, both of which the Hit leg
    // skips.
    assert!(
        hit_elapsed < miss_elapsed,
        "a cold-base HIT ({hit_elapsed:?}) must be faster than a cold-boot MISS \
         ({miss_elapsed:?}) — no second cold boot should have happened",
    );

    // (a) the overlay dedupes against the cold base: substantially fewer
    // new chunk blobs are uploaded for the hit-leg overlay than were
    // uploaded minting the cold base itself (the full guest image,
    // disk + mem). The bar is HALF, deliberately loose: the overlay's
    // novel content is the hook-dirtied marker PLUS whatever pages the
    // guest kernel dirtied on its own during the restore→hook window
    // (timers, kswapd, journal writeback — nondeterministic), and diff
    // granularity is per-chunk, so one dirtied page pays a whole chunk.
    // Observed on the CI runner: 38 vs 145 puts (74% dedup) — a broken
    // diff (a second Full) re-puts ~everything and fails HALF by a
    // mile, which is the regression this guards; a tighter ratio just
    // flakes on the background-dirtying noise floor (a 1/4 bar failed
    // CI at 26% on an otherwise-correct diff).
    assert!(
        hit_puts * 2 < miss_puts,
        "the hit-leg overlay ({hit_puts} new chunk puts) must dedupe against the cold \
         base's chunks — expected well under half of the miss leg's {miss_puts} puts \
         (a diff, not a second Full dump)",
    );
}

// ---- test harness (mirrors `warm_hook_capture.rs`'s `TestEnv` — test
// fixtures don't cross crate boundaries, so this is intentionally
// duplicated rather than shared) -------------------------------------

struct TestEnv {
    kernel: std::path::PathBuf,
    staged: common::StagedAgentdBundle,
    busybox: std::path::PathBuf,
    work: tempfile::TempDir,
    images: tempfile::TempDir,
    /// The plain (uncounted) blob store `bake()` uses to seed the
    /// baked rootfs's disk manifest — `pooled()` wraps a FRESH
    /// `CountingBlobStorage` over the SAME underlying blob so both
    /// capture legs' chunk uploads land in one place, but only the
    /// pooled backend's own puts get counted.
    blob: Arc<dyn BlobStorage>,
    chunk_store: ChunkStore,
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
        for bin in ["firecracker", "mke2fs", "mksquashfs"] {
            let missing = std::env::var_os("PATH")
                .map(|p| !std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
                .unwrap_or(true);
            if missing {
                eprintln!("SKIP: {bin} not on PATH");
                return None;
            }
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
        let blob: Arc<dyn BlobStorage> = Arc::new(engram_storage_local::LocalBlobStorage::new(
            work.path().join("blob"),
        ));
        let chunk_store = ChunkStore::new(blob.clone());
        std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");
        Some(Self {
            kernel,
            staged,
            busybox,
            work,
            images,
            blob,
            chunk_store,
        })
    }

    /// `PooledBackend` with dirty-page tracking + a checkpoint_dir wired
    /// (so `capture_phase`'s chain gate actually engages — the property
    /// this test proves depends on Full-vs-Diff branching being live,
    /// unlike the unit-level mock tests in `pooled_backend.rs`, which
    /// deliberately leave it unwired). Blob storage is wrapped to count
    /// chunk uploads.
    fn pooled(&self, puts: Arc<AtomicUsize>) -> Arc<PooledBackend> {
        let mut cfg = FirecrackerConfig::with_kernel(self.kernel.clone());
        cfg.bundle_dir = self.staged.bundle_dir.clone();
        cfg.net_pool = None;
        cfg.restore_mode = RestoreMode::File;
        cfg.track_dirty_pages = true;
        cfg.default_boot_args =
            "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
        let inner = Arc::new(FirecrackerBackend::new(self.work.path(), cfg));
        let counting_blob: Arc<dyn BlobStorage> = Arc::new(CountingBlobStorage {
            inner: self.blob.clone(),
            puts,
        });
        let counting_store = ChunkStore::new(counting_blob);
        let mut cache_cfg =
            engram_chunk_store::cache::ChunkCacheConfig::new(self.work.path().join("chunk-cache"));
        cache_cfg.budget_bytes = 1024 * 1024 * 1024;
        Arc::new(
            PooledBackend::new(inner)
                .with_chunk_store(counting_store, self.work.path().join("materialize"))
                .with_chunk_cache(engram_chunk_store::ChunkCache::new(cache_cfg))
                .with_checkpoint_dir(self.work.path().join("checkpoints")),
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
            image: "engram-cold-base-test".into(),
            rootfs_source: Some(rootfs.to_path_buf()),
            image_uri: None,
            rootfs_manifest: None,
            cpu: CpuLimit { vcpus: 1 },
            // 256 MiB guest RAM: big enough that the marker (32 KiB) is
            // a small fraction of the image (the diff-vs-full asymmetry
            // this test proves needs that headroom), small enough to
            // keep the cold-boot leg fast.
            memory: engram_core::types::sandbox::MemoryLimit { max_mib: 256 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: HashMap::new(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: vec![self.staged.agentd_slot()],
        }
    }
}
