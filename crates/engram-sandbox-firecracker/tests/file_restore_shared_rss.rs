//! ADR 0022 Option A go/no-go spike + regression gate: **N File-backend
//! restores off one memory.bin share clean pages via the host page
//! cache** (`MAP_PRIVATE` of one inode → one physical copy per page).
//!
//! This is the density thesis in miniature:
//!
//!   - Bake a tiny rootfs whose init fills a 64 MiB tmpfs blob (guest
//!     RAM) and re-reads it every second — a guest working set that
//!     every sibling re-touches after restore, so the shared pages
//!     actually fault in and become measurable. The init is baked in
//!     via the image builder (`mke2fs -d` builds the whole tree at FS
//!     creation, computing correct `metadata_csum` for every block) —
//!     NOT a post-hoc `debugfs write`, which lands the file's data
//!     blocks with mismatched checksums on some e2fsprogs versions
//!     (1.47.0 on ubuntu-24.04 CI runners): the guest kernel then
//!     fails to `execve` the injected init with `EBADMSG` and panics
//!     ("Requested init … failed (error -74)") before it can be
//!     snapshotted. The bake path is the one `exec_real_vm` /
//!     `diff_snapshot` use, and it boots cleanly on every runner.
//!   - Snapshot once, destroy the source, then restore **3** VMs from
//!     the same snapshot dir (File mode — the default `RestoreMode`).
//!     All 3 mmap the SAME memory.bin inode.
//!   - Read `/proc/<fc-pid>/smaps_rollup` per VM: with page-cache
//!     sharing working, pages of the common working set have
//!     mapcount≈3, so `Pss ≈ Rss/3` for that segment and the summed Pss
//!     across siblings sits well under summed Rss. Without sharing
//!     (e.g. a future FC regression to eager copy), Pss ≈ Rss and the
//!     assertion fails.
//!
//! Numbers print as `SPIKE:` lines — they are ADR 0022's shared-RSS
//! measurement and ADR 0028 P1's restore-latency datapoint.
//!
//! Gating: Linux + KVM + firecracker + Docker + `mke2fs` (e2fsprogs).
//! Run:
//!
//! ```sh
//! bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh file_restore_shared_rss
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use engram_chunk_store::{ChunkCache, ChunkCacheConfig, ChunkStore, ManifestKind};
use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_image_builder::{BuildRequest, Builder, DockerCli, Format};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, RestoreMode};
use engram_storage_local::LocalBlobStorage;
use tempfile::TempDir;

use common::{fc_preflight, require_bin};

const SIBLINGS: usize = 3;
const BLOB_MIB: u64 = 64;

/// Init baked into the rootfs: build the blob in tmpfs (guest RAM),
/// then re-read it forever so restored siblings keep faulting the same
/// guest-physical pages back in from memory.bin.
///
/// `/bin/sh` (not bash) for maximal portability, and — critically — the
/// script can NEVER exit: a pid-1 that returns triggers `panic=1
/// reboot=k`, which kills FC and makes the host-side `snapshot()` hit a
/// dead socket. So every step is best-effort and the final loop is
/// unconditional; a failed mount/fill then surfaces as the host-side
/// RSS-floor assertion (the honest signal), not a dead VM. Baked via
/// `mke2fs -d` (see the module docs) so the guest kernel can actually
/// `execve` it.
const SPIKE_INIT: &str = "#!/bin/sh\n\
export PATH=/usr/sbin:/usr/bin:/sbin:/bin\n\
mount -t proc proc /proc 2>/dev/null || true\n\
mount -t devtmpfs dev /dev 2>/dev/null || true\n\
mount -t tmpfs -o size=128m tmpfs /tmp 2>/dev/null || true\n\
head -c 67108864 /dev/urandom > /tmp/blob 2>/dev/null || true\n\
while true; do cat /tmp/blob > /dev/null 2>&1 || true; sleep 1; done\n";

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker + mke2fs; bakes a rootfs and boots microVMs"]
async fn file_backend_siblings_share_clean_pages() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_bin("docker") || !require_bin("mke2fs") {
        return;
    }

    // Keep the jail dir on failure so we can read firecracker.log (which
    // carries the guest serial console — lib.rs funnels console=ttyS0 +
    // FC's own stderr into jail_dir/firecracker.log). Without this, a
    // dead guest surfaces only as a bare "Connection refused" on the
    // snapshot socket with no clue why it died.
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    // ---- 1. Bake a self-driving rootfs (init baked in via mke2fs -d) ----
    let baked = bake_spike_rootfs().await;

    // ---- 2. Boot, let the blob fill, snapshot, destroy ----
    let work = tempfile::tempdir().expect("work dir");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = None; // unprivileged test — see lifecycle.rs
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/spike-init.sh".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "fc-shared-rss-test".into(),
        rootfs_source: Some(baked.rootfs.clone()),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 256 },
        disk: DiskLimit { max_gib: 2 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
    };
    let source = backend.create(spec).await.expect("create");
    // Boot + 64 MiB urandom fill + at least one read pass — poll the source's
    // own RSS gauge until the blob is resident instead of sleeping a fixed 8s.
    wait_blob_resident(work.path(), &source.to_string(), Duration::from_secs(30)).await;
    // If the guest didn't survive to be snapshotted, the FC API socket
    // is gone (panic=1 reboot=k → KVM reset → FC exits) and we'd get a
    // bare ECONNREFUSED. Surface the guest's own panic reason from
    // firecracker.log before failing — a dead init/boot is a real
    // signal, not something to skip past.
    let metadata = match backend.snapshot(source).await {
        Ok(m) => m,
        Err(e) => {
            dump_fc_logs(work.path());
            let _ = backend.destroy(source).await;
            panic!(
                "snapshot failed — source guest did not survive to be snapshotted: {e}\n\
                 (see the dumped firecracker.log above for the guest console / kernel panic)"
            );
        }
    };
    backend.destroy(source).await.expect("destroy source");

    // ---- 3. Restore N siblings off the same memory.bin ----
    // Serial restores (the per-snapshot canonical vsock UDS path is
    // re-bound by each load — last binder owns it; harmless here since
    // the workload is self-driving and the test does no host→guest
    // exec). All siblings stay alive together: that's the sharing
    // condition.
    let mut vms = Vec::new();
    for i in 0..SIBLINGS {
        let t = Instant::now();
        let id = match backend.restore(metadata.clone()).await {
            Ok(id) => id,
            Err(e) => {
                dump_fc_logs(work.path());
                for v in &vms {
                    let _ = backend.destroy(*v).await;
                }
                panic!(
                    "sibling {i} failed to restore off the shared memory.bin: {e:?}\n\
                     (see the dumped firecracker.log above for the guest console)"
                );
            }
        };
        eprintln!(
            "SPIKE: restore sibling {i} took {} ms",
            t.elapsed().as_millis()
        );
        vms.push(id);
    }

    // Wait until every sibling's read loop has faulted the common working set
    // in (RSS > blob) — the condition the measurement below asserts on —
    // rather than a fixed sweep delay.
    wait_all_resident(work.path(), &vms, Duration::from_secs(30)).await;

    // ---- 4. Measure ----
    let mut total_rss = 0u64;
    let mut total_pss = 0u64;
    for (i, id) in vms.iter().enumerate() {
        let pid = fc_pid_for(work.path(), &id.to_string());
        let m = smaps_rollup(pid);
        eprintln!(
            "SPIKE: sibling {i} pid {pid}: rss {} KiB, pss {} KiB, shared_clean {} KiB, \
             private_dirty {} KiB",
            m.rss_kb, m.pss_kb, m.shared_clean_kb, m.private_dirty_kb,
        );
        // Guards the workload, not just the mechanism: if the init's
        // tmpfs mount or blob fill silently failed, the guest working
        // set collapses to the ~20 MiB boot set and this test would
        // "pass" while measuring almost nothing. The blob alone is
        // 64 MiB of guest RAM that the read loop keeps resident.
        assert!(
            m.rss_kb > BLOB_MIB * 1024,
            "sibling {i} rss {} KiB < blob size — in-guest workload didn't run \
             (tmpfs mount or blob fill failed)",
            m.rss_kb,
        );
        total_rss += m.rss_kb;
        total_pss += m.pss_kb;
    }
    let pct = total_pss * 100 / total_rss.max(1);
    eprintln!(
        "SPIKE: Σpss/Σrss = {total_pss}/{total_rss} KiB = {pct}% \
         (no sharing ⇒ ~100%; perfect 3-way sharing of everything ⇒ ~33%)"
    );

    for id in &vms {
        backend.destroy(*id).await.expect("destroy sibling");
    }

    // The blob alone is 64 MiB of a sibling's ~100–150 MiB faulted set;
    // 3-way sharing of just the blob already pulls the ratio under
    // ~80%. Lenient so CI host variance (page-cache pressure, kernel
    // accounting drift) doesn't flake — the printed number is the
    // measurement; the assertion guards the *mechanism*.
    assert!(
        pct < 80,
        "File-backend restores show no meaningful page sharing \
         (Σpss/Σrss = {pct}%) — MAP_PRIVATE page-cache sharing broken?",
    );
}

/// ADR 0022 Option A: the *production* density path, end to end —
/// (a) materialize the per-template base memfile **from a chunk manifest**
/// (the residency step `image_prefetch` runs), and (b) restore N siblings
/// through the **File-mode base-create** bifurcation (`restore_fresh` with
/// `base_restore_mode = File` while `restore_mode = Uffd` — exactly prod's
/// "File for create, UFFD for resume"), asserting both page sharing and
/// per-sibling restore latency.
///
/// Distinct from `file_backend_siblings_share_clean_pages` (which restores
/// the snapshot's own freshly-written memory.bin via the resume flavor):
/// here the memory.bin every sibling maps is **reconstructed from chunks**,
/// proving the residency-materialized file is byte-faithful AND shareable,
/// and the restore goes through the base-create mode bifurcation, not the
/// global default. The resume-stays-UFFD half of the invariant is covered
/// by the `effective_restore_mode_bifurcates_create_vs_resume` unit test +
/// `snapshot_uffd.rs`.
#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker + mke2fs; bakes a rootfs and boots microVMs"]
async fn file_backend_base_create_shares_residency_memfile() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_bin("docker") || !require_bin("mke2fs") {
        return;
    }
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    let baked = bake_spike_rootfs().await;

    // Prod-shaped config: idle-resume on UFFD, base session.create flipped
    // to File (ADR 0022). `restore_fresh` must therefore pick File even
    // though `restore_mode` is Uffd — that's the bifurcation under test.
    let work = tempfile::tempdir().expect("work dir");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = None;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/spike-init.sh".into();
    // ADR 0045 D3: fresh-create resolves to File whenever no substrate
    // dir is configured — the bifurcation is derived, not configured.
    cfg.restore_mode = RestoreMode::Uffd;
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "fc-shared-rss-test".into(),
        rootfs_source: Some(baked.rootfs.clone()),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 256 },
        disk: DiskLimit { max_gib: 2 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
    };
    let source = backend.create(spec).await.expect("create");
    wait_blob_resident(work.path(), &source.to_string(), Duration::from_secs(30)).await;
    let metadata = match backend.snapshot(source).await {
        Ok(m) => m,
        Err(e) => {
            dump_fc_logs(work.path());
            let _ = backend.destroy(source).await;
            panic!("snapshot failed — source guest did not survive: {e}");
        }
    };
    backend.destroy(source).await.expect("destroy source");

    // ---- Residency materialize: chunk memory.bin, delete it, rebuild it
    // from the manifest (what image_prefetch does at residency) ----
    let mem_path = backend.snapshot_path_for(metadata.id).join("memory.bin");
    let original = std::fs::read(&mem_path).expect("read captured memory.bin");

    let store_root = tempfile::tempdir().expect("store root");
    let blob: std::sync::Arc<dyn engram_core::traits::BlobStorage> =
        std::sync::Arc::new(LocalBlobStorage::new(store_root.path().join("blob")));
    let chunk_store = ChunkStore::new(blob);
    let cache = ChunkCache::new(ChunkCacheConfig {
        root: store_root.path().join("cache"),
        budget_bytes: 4 * 1024 * 1024 * 1024,
        sweep_debounce_ms: 0,
    });
    let memory_manifest = chunk_store
        .chunk_file(&mem_path, ManifestKind::Memory, None)
        .await
        .expect("chunk memory.bin");

    std::fs::remove_file(&mem_path).expect("delete captured memory.bin");
    let mat = Instant::now();
    chunk_store
        .materialize_to_file_cached(&memory_manifest, &mem_path, &cache)
        .await
        .expect("re-materialize memory.bin from manifest");
    eprintln!(
        "SPIKE: residency materialize-from-manifest took {} ms",
        mat.elapsed().as_millis()
    );
    assert_eq!(
        std::fs::read(&mem_path).expect("read rematerialized memory.bin"),
        original,
        "residency-materialized memory.bin must be byte-identical to the captured one",
    );

    // ---- Restore N siblings via File-mode base-create (restore_fresh) ----
    let mut vms = Vec::new();
    let mut latencies_ms = Vec::new();
    for i in 0..SIBLINGS {
        let t = Instant::now();
        let id = match backend.restore_fresh(metadata.clone(), Vec::new()).await {
            Ok(id) => id,
            Err(e) => {
                dump_fc_logs(work.path());
                for v in &vms {
                    let _ = backend.destroy(*v).await;
                }
                panic!("sibling {i} failed File-mode base-create off the residency memfile: {e:?}");
            }
        };
        let ms = t.elapsed().as_millis();
        eprintln!("SPIKE: file-restore sibling {i} = {ms} ms");
        latencies_ms.push(ms);
        vms.push(id);
    }
    wait_all_resident(work.path(), &vms, Duration::from_secs(30)).await;

    // ---- Measure density + latency ----
    let mut total_rss = 0u64;
    let mut total_pss = 0u64;
    for (i, id) in vms.iter().enumerate() {
        let pid = fc_pid_for(work.path(), &id.to_string());
        let m = smaps_rollup(pid);
        eprintln!(
            "SPIKE: sibling {i} pid {pid}: rss {} KiB, pss {} KiB, shared_clean {} KiB, \
             private_dirty {} KiB",
            m.rss_kb, m.pss_kb, m.shared_clean_kb, m.private_dirty_kb,
        );
        assert!(
            m.rss_kb > BLOB_MIB * 1024,
            "sibling {i} rss {} KiB < blob size — in-guest workload didn't run",
            m.rss_kb,
        );
        // Each sibling's Pss sits well below its Rss — the base working
        // set is one physical copy split across the siblings mapping the
        // shared memfile inode (≈3-way ⇒ Pss≈Rss/3). We assert on Pss/Rss
        // (the density signal) rather than Shared_Clean specifically:
        // because the residency step just *wrote* this memfile, its
        // page-cache pages are still dirty (pending writeback) when the
        // guests fault them, so the kernel classifies the shared pages as
        // Shared_Dirty, not Shared_Clean — identical physical RAM, just a
        // different smaps bucket. (In prod the memfile is materialized at
        // residency long before the first session, so writeback has run
        // and the same pages show up as Shared_Clean.)
        assert!(
            m.pss_kb * 2 < m.rss_kb,
            "sibling {i} Pss {} KiB not << Rss {} KiB — residency memfile not shared?",
            m.pss_kb,
            m.rss_kb,
        );
        total_rss += m.rss_kb;
        total_pss += m.pss_kb;
    }
    let pct = total_pss * 100 / total_rss.max(1);
    latencies_ms.sort_unstable();
    let median = latencies_ms[latencies_ms.len() / 2];
    eprintln!(
        "SPIKE: base-create Σpss/Σrss = {total_pss}/{total_rss} KiB = {pct}% \
         (no sharing ⇒ ~100%; perfect 3-way ⇒ ~33%); restore median {median} ms"
    );

    for id in &vms {
        backend.destroy(*id).await.expect("destroy sibling");
    }

    assert!(
        pct < 80,
        "File-mode base-create off the residency memfile shows no page sharing \
         (Σpss/Σrss = {pct}%)",
    );
}

/// ADR 0045 D3 parity gate: **substrate (v2b) cold boot vs File-mode cold
/// boot, apples-to-apples** — same baked workload, same snapshot, same
/// host, measured in one run:
///
///   - Arm A (today's prod default): `restore_fresh` through the File-mode
///     base-create bifurcation off the captured memory.bin.
///   - Arm B (the substrate): `restore_fresh` with `restore_mode = Uffd` +
///     `uffd_base_dir` on the FORKED FC — guest memory `MAP_PRIVATE` of the
///     per-template base shm, canonical faults CONTINUE-shared.
///
/// Gates (ADR 0045 D3): density — substrate Σpss/Σrss must be within 10
/// points of File mode's (both share the canonical working set); the
/// printed latencies are the ADR's measured numbers (the assertion bound
/// is generous — the decision is made on the prints).
///
/// Skips cleanly without `ENGRAM_FC_FORK_BIN` (the substrate is fork-only)
/// or a built `engram-uffd-handler`.
#[tokio::test]
#[ignore = "requires Linux + KVM + ENGRAM_FC_FORK_BIN + Docker + mke2fs + built engram-uffd-handler"]
async fn substrate_base_create_density_and_latency_parity() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_bin("docker") || !require_bin("mke2fs") {
        return;
    }
    let fork_bin = match std::env::var("ENGRAM_FC_FORK_BIN") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => {
            eprintln!("SKIP: ENGRAM_FC_FORK_BIN not set (substrate is fork-only)");
            return;
        }
    };
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let handler = Path::new(&manifest_dir).join("../../target/debug/engram-uffd-handler");
    if !handler.exists() {
        eprintln!("SKIP: engram-uffd-handler not built");
        return;
    }
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    let baked = bake_spike_rootfs().await;

    // ---- Source boot + snapshot (shared by both arms) ----
    let work = tempfile::tempdir().expect("work dir");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
    cfg.net_pool = None;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/spike-init.sh".into();
    // The fork is a verified drop-in; use it for BOTH arms so the binary
    // isn't a variable in the comparison.
    cfg.firecracker_bin = fork_bin.clone();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "fc-substrate-parity-test".into(),
        rootfs_source: Some(baked.rootfs.clone()),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 256 },
        disk: DiskLimit { max_gib: 2 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
    };
    let source = backend.create(spec).await.expect("create");
    wait_blob_resident(work.path(), &source.to_string(), Duration::from_secs(30)).await;
    let metadata = match backend.snapshot(source).await {
        Ok(m) => m,
        Err(e) => {
            dump_fc_logs(work.path());
            let _ = backend.destroy(source).await;
            panic!("snapshot failed — source guest did not survive: {e}");
        }
    };
    backend.destroy(source).await.expect("destroy source");

    // Chunk memory.bin where the spawned handler will find it (the
    // ENGRAM_LOCAL_PATH/blobs layout — see snapshot_uffd.rs).
    let local_path = work.path().join("local-state");
    std::env::set_var("ENGRAM_BLOB_BACKEND", "local");
    std::env::set_var("ENGRAM_LOCAL_PATH", &local_path);
    let blob: std::sync::Arc<dyn engram_core::traits::BlobStorage> =
        std::sync::Arc::new(LocalBlobStorage::new(local_path.join("blobs")));
    let chunk_store = ChunkStore::new(blob);
    let mem_path = backend.snapshot_path_for(metadata.id).join("memory.bin");
    let chunked = chunk_store
        .chunk_file(&mem_path, ManifestKind::Memory, None)
        .await
        .expect("chunk memory.bin");
    let manifest_ref = engram_core::types::manifest::ManifestRef::new();
    chunk_store
        .put_manifest(manifest_ref, &chunked)
        .await
        .expect("put memory manifest");
    // The restore path trusts the on-disk sidecar manifest.json over the
    // passed metadata — patch memory_manifest into it (see snapshot_uffd.rs).
    let manifest_json = backend.snapshot_path_for(metadata.id).join("manifest.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_json).expect("read mj")).expect("parse mj");
    value.as_object_mut().expect("mj root is an object").insert(
        "memory_manifest".into(),
        serde_json::to_value(manifest_ref).expect("serialize mref"),
    );
    std::fs::write(
        &manifest_json,
        serde_json::to_vec_pretty(&value).expect("re-serialize mj"),
    )
    .expect("write patched mj");

    // One measurement arm: restore SIBLINGS via `restore_fresh` on the
    // given backend, settle, measure (pct, median restore ms), destroy.
    async fn measure_arm(
        label: &str,
        backend: &FirecrackerBackend,
        metadata: &engram_core::types::snapshot::SnapshotMetadata,
        work_path: &Path,
    ) -> (u64, u128) {
        let mut vms = Vec::new();
        let mut latencies_ms = Vec::new();
        for i in 0..SIBLINGS {
            let t = Instant::now();
            let id = match backend.restore_fresh(metadata.clone(), Vec::new()).await {
                Ok(id) => id,
                Err(e) => {
                    dump_fc_logs(work_path);
                    for v in &vms {
                        let _ = backend.destroy(*v).await;
                    }
                    panic!("{label} sibling {i} failed to restore: {e:?}");
                }
            };
            let ms = t.elapsed().as_millis();
            eprintln!("SPIKE: {label} restore sibling {i} = {ms} ms");
            latencies_ms.push(ms);
            vms.push(id);
        }
        // Settle until every sibling's working set is faulted in (RSS > blob).
        //
        // Do NOT bail early on a flat-but-low reading. A sibling whose UFFD
        // page-in ramps slowly, or whose resumed read-loop is briefly stalled
        // under concurrent restore + CI load, can sit at a stable low RSS for a
        // sample or two before catching up. The old `rss == last_rss` early-break
        // read that single flat sample as "wedged" and flaked this test; we now
        // let the full budget play out (only the all-faulted-in success case
        // exits early). The budget is generous (60s) because it only matters for
        // the slow case; the common case breaks out the moment every sibling
        // crosses the blob.
        //
        // A single straggler that never catches up does NOT fail the run. The
        // substrate arm's faults go through a userspace handler, so under a
        // contended `--test-threads` CI box one sibling's vCPU/handler can be
        // starved into a flat sub-blob RSS for the whole budget while its
        // siblings (busy-faulting the shared base) race ahead — exactly the
        // flake observed in CI (run 28453272345: 2 siblings at ~110 MiB, 1 stuck
        // at ~18 MiB). That is a measurement-environment artifact, not a
        // substrate regression: the density ratio we actually gate on is taken
        // across the *aggregate* working set, so the post-loop guard below is on
        // Σrss (tolerates one straggler) rather than per-sibling. A real
        // fleet-wide "workload never ran" still trips it; see there.
        for round in 0..60 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let rss: Vec<u64> = vms
                .iter()
                .map(|id| smaps_rollup(fc_pid_for(work_path, &id.to_string())).rss_kb)
                .collect();
            eprintln!("SPIKE: {label} settle round {round}: rss KiB = {rss:?}");
            if rss.iter().all(|r| *r > BLOB_MIB * 1024) {
                break;
            }
        }
        let (mut total_rss, mut total_pss) = (0u64, 0u64);
        let mut faulted_in = 0usize;
        for (i, id) in vms.iter().enumerate() {
            let pid = fc_pid_for(work_path, &id.to_string());
            let m = smaps_rollup(pid);
            eprintln!(
                "SPIKE: {label} sibling {i} pid {pid}: rss {} KiB, pss {} KiB, \
                 shared_clean {} KiB, private_dirty {} KiB",
                m.rss_kb, m.pss_kb, m.shared_clean_kb, m.private_dirty_kb,
            );
            if m.rss_kb > BLOB_MIB * 1024 {
                faulted_in += 1;
            } else {
                // Not fatal on its own — a contention-starved straggler (see the
                // settle-loop note). Fold it into the aggregate guard below; its
                // low, poorly-shared RSS only makes the density ratio *more*
                // conservative, never a false pass.
                eprintln!(
                    "SPIKE: {label} sibling {i} rss {} KiB < blob size — lagged \
                     (likely CI contention), folding into the aggregate",
                    m.rss_kb,
                );
            }
            total_rss += m.rss_kb;
            total_pss += m.pss_kb;
        }
        // Workload-ran guard, aggregated. The in-guest blob fill + read loop is
        // what makes the shared working set measurable; if it silently never
        // ran (tmpfs mount / blob fill failed everywhere), every sibling
        // collapses to its ~20 MiB boot set (~60 MiB total) and the density
        // number is meaningless. Require the fleet to have faulted in at least
        // a blob's worth per sibling in aggregate (3 × 64 MiB): that trips hard
        // on a fleet-wide "didn't run" AND on a real "substrate can't serve
        // concurrent restores" (≥2 of 3 stuck ⇒ total below floor), while
        // tolerating the single starved straggler that flaked this test.
        let aggregate_floor = SIBLINGS as u64 * BLOB_MIB * 1024;
        if total_rss <= aggregate_floor {
            // Dump the guest console + handler logs so a real failure here is
            // root-causable instead of a blind "didn't run".
            dump_fc_logs(work_path);
            for v in vms.iter() {
                let _ = backend.destroy(*v).await;
            }
            panic!(
                "{label}: only {faulted_in}/{SIBLINGS} siblings faulted in their working set \
                 (Σrss {total_rss} KiB ≤ floor {aggregate_floor} KiB) — in-guest workload \
                 didn't run (see the dumped firecracker / uffd-handler logs above)"
            );
        }
        if faulted_in < SIBLINGS {
            eprintln!(
                "SPIKE: {label}: {faulted_in}/{SIBLINGS} siblings fully faulted in \
                 (Σrss {total_rss} KiB > floor {aggregate_floor} KiB — measurement still valid)"
            );
        }
        for id in &vms {
            backend.destroy(*id).await.expect("destroy sibling");
        }
        let pct = total_pss * 100 / total_rss.max(1);
        latencies_ms.sort_unstable();
        let median = latencies_ms[latencies_ms.len() / 2];
        eprintln!("SPIKE: {label}: Σpss/Σrss = {pct}%, restore median {median} ms");
        (pct, median)
    }

    // ---- Arm A: File-mode base-create (today's prod default) ----
    let mut cfg_file = FirecrackerConfig::with_kernel(env.kernel.clone());
    cfg_file.net_pool = None;
    cfg_file.default_boot_args =
        "console=ttyS0 reboot=k panic=1 pci=off init=/spike-init.sh".into();
    cfg_file.firecracker_bin = fork_bin.clone();
    cfg_file.restore_mode = RestoreMode::Uffd;
    let backend_file = FirecrackerBackend::new(work.path(), cfg_file);
    let (pct_file, median_file) =
        measure_arm("file-mode", &backend_file, &metadata, work.path()).await;

    // ---- Arm B: the substrate (Uffd + base shm on the fork) ----
    let base_dir = PathBuf::from(format!(
        "/dev/shm/engram-parity-test-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&base_dir).expect("create base dir");
    let mut cfg_sub = FirecrackerConfig::with_kernel(env.kernel.clone());
    cfg_sub.net_pool = None;
    cfg_sub.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/spike-init.sh".into();
    cfg_sub.firecracker_bin = fork_bin;
    cfg_sub.uffd_handler_bin = handler;
    cfg_sub.restore_mode = RestoreMode::Uffd;
    cfg_sub.uffd_base_dir = Some(base_dir.clone());
    let backend_sub = FirecrackerBackend::new(work.path(), cfg_sub);
    let mut metadata_sub = metadata.clone();
    metadata_sub.memory_manifest = Some(manifest_ref);
    let (pct_sub, median_sub) =
        measure_arm("substrate", &backend_sub, &metadata_sub, work.path()).await;
    let _ = std::fs::remove_dir_all(&base_dir);

    eprintln!(
        "SPIKE: D3 PARITY — density file={pct_file}% substrate={pct_sub}% \
         | restore median file={median_file}ms substrate={median_sub}ms"
    );

    // The D3 density gate: the substrate must share what File mode shares.
    // +10 points of slack for accounting drift between arms.
    assert!(
        pct_sub <= pct_file + 10,
        "substrate density regressed vs File mode: Σpss/Σrss {pct_sub}% vs {pct_file}%",
    );
    // Generous latency bound — the printed medians are the ADR's measured
    // numbers; this assertion only catches a pathological regression
    // (e.g. the lazy path degenerating into a synchronous full read).
    assert!(
        median_sub <= median_file + 2000,
        "substrate restore median {median_sub} ms wildly above File mode {median_file} ms",
    );
}

/// Bake the self-driving spike rootfs (init baked in via `mke2fs -d`; see
/// the module docs for why post-hoc `debugfs write` is avoided). Returns
/// the ext4 rootfs path plus the tempdirs backing it — the caller must
/// keep `Baked` alive for as long as the rootfs is in use.
struct Baked {
    rootfs: PathBuf,
    _src: TempDir,
    _images: TempDir,
    _chunk_root: TempDir,
}

async fn bake_spike_rootfs() -> Baked {
    let src = tempfile::tempdir().expect("source dir");
    std::fs::write(src.path().join("spike-init.sh"), SPIKE_INIT).expect("write init");
    std::fs::write(
        src.path().join("Dockerfile"),
        "FROM debian:bookworm-slim\n\
         COPY spike-init.sh /spike-init.sh\n\
         RUN chmod 0755 /spike-init.sh\n",
    )
    .expect("write Dockerfile");
    std::fs::write(
        src.path().join("engram.toml"),
        "name = \"fc-shared-rss-test\"\n",
    )
    .expect("write engram.toml");

    let images = tempfile::tempdir().expect("images dir");
    let chunk_root = tempfile::tempdir().expect("chunk store root");
    let blob: std::sync::Arc<dyn engram_core::traits::BlobStorage> =
        std::sync::Arc::new(LocalBlobStorage::new(chunk_root.path().to_path_buf()));
    let chunk_store = ChunkStore::new(blob);
    let baker = Builder::new(DockerCli::new(), chunk_store);
    let outcome = baker
        .build(&BuildRequest {
            source: src.path().to_path_buf(),
            repo: "engram-shared-rss-test".into(),
            tag: "warm-1".into(),
            images_dir: images.path().to_path_buf(),
            format: Format::Ext4,
            agent_injection: None,
        })
        .await
        .expect("ext4 bake");
    Baked {
        rootfs: outcome.rootfs_path,
        _src: src,
        _images: images,
        _chunk_root: chunk_root,
    }
}

/// Walk the work dir for every `firecracker.log` and `uffd-handler.log`
/// and print its tail. FC funnels the guest serial console
/// (`console=ttyS0`) plus its own stderr into `firecracker.log` (see
/// `lib.rs` jail setup), so when a guest panics on boot/init this is the
/// only place the reason appears. `uffd-handler.log` is the substrate
/// path's twin: a starved or stuck page-fault handler (the substrate
/// arm's failure mode) surfaces only there, not on the guest console.
fn dump_fc_logs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            dump_fc_logs(&path);
        } else if path
            .file_name()
            .is_some_and(|n| n == "firecracker.log" || n == "uffd-handler.log")
        {
            match std::fs::read_to_string(&path) {
                Ok(content) => eprintln!(
                    "--- {} ---\n{}\n--- end ---",
                    path.display(),
                    content.trim_end()
                ),
                Err(e) => eprintln!("(could not read {}: {e})", path.display()),
            }
        }
    }
}

/// Find the firecracker process whose cmdline mentions this sandbox's
/// jail dir (the API socket path embeds the sandbox id). `None` if no live
/// process matches — e.g. the guest panicked on boot and FC exited.
fn fc_pid_opt(work_dir: &Path, sandbox_id: &str) -> Option<u32> {
    let needle = work_dir.join(sandbox_id).to_string_lossy().into_owned();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if String::from_utf8_lossy(&cmdline).contains(&needle) {
            return Some(pid);
        }
    }
    None
}

/// Strict variant for the measurement code: a missing process there is a
/// real failure, not a transient readiness state.
fn fc_pid_for(work_dir: &Path, sandbox_id: &str) -> u32 {
    fc_pid_opt(work_dir, sandbox_id)
        .unwrap_or_else(|| panic!("no firecracker process found for sandbox {sandbox_id}"))
}

/// Non-panicking RSS read for the readiness polls (the process may not exist
/// yet / any more). The measurement path uses `smaps_rollup` instead, which
/// panics on a missing field — there, a missing gauge is a real bug.
fn rss_kb_opt(pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).ok()?;
    text.lines()
        .find(|l| l.starts_with("Rss:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

/// Wait until the source guest's blob workload is resident — its FC process
/// RSS has crossed the blob size, i.e. tmpfs mounted + 64 MiB blob filled +
/// faulted into guest RAM. This is the exact gauge the post-restore siblings
/// are asserted on, so it's the honest "ready to snapshot" signal; we pay the
/// real (sub-10s) boot+fill latency instead of a fixed 8s. On a dead guest
/// the poll simply times out and the caller's `snapshot()` surfaces the
/// console via `dump_fc_logs`.
async fn wait_blob_resident(work: &Path, id: &str, timeout: Duration) {
    common::poll_until(timeout, Duration::from_millis(250), || {
        fc_pid_opt(work, id)
            .and_then(rss_kb_opt)
            .is_some_and(|rss| rss > BLOB_MIB * 1024)
    })
    .await;
}

/// Wait until EVERY sibling's read loop has faulted the common working set in
/// (RSS > blob) — the condition the density measurement asserts on — instead
/// of a fixed "let them sweep" delay.
async fn wait_all_resident<I: std::fmt::Display>(work: &Path, ids: &[I], timeout: Duration) {
    common::poll_until(timeout, Duration::from_secs(1), || {
        ids.iter().all(|id| {
            fc_pid_opt(work, &id.to_string())
                .and_then(rss_kb_opt)
                .is_some_and(|rss| rss > BLOB_MIB * 1024)
        })
    })
    .await;
}

struct SmapsRollup {
    rss_kb: u64,
    pss_kb: u64,
    shared_clean_kb: u64,
    private_dirty_kb: u64,
}

fn smaps_rollup(pid: u32) -> SmapsRollup {
    let text =
        std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup")).expect("read smaps_rollup");
    let field = |name: &str| -> u64 {
        text.lines()
            .find(|l| l.starts_with(name))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| panic!("field {name} missing in smaps_rollup:\n{text}"))
    };
    SmapsRollup {
        rss_kb: field("Rss:"),
        pss_kb: field("Pss:"),
        shared_clean_kb: field("Shared_Clean:"),
        private_dirty_kb: field("Private_Dirty:"),
    }
}
