//! ADR 0022 Option A go/no-go spike + regression gate: **N File-backend
//! restores off one memory.bin share clean pages via the host page
//! cache** (`MAP_PRIVATE` of one inode → one physical copy per page).
//!
//! This is the density thesis in miniature:
//!
//!   - Bake a tiny rootfs whose init fills a 64 MiB tmpfs blob (guest
//!     RAM) and re-reads it every second — a guest working set that
//!     every sibling re-touches after restore, so the shared pages
//!     actually fault in and become measurable. The init is packed in
//!     at bake time (the packer builds the whole tree at FS
//!     creation, computing correct `metadata_csum` for every block) —
//!     NOT a post-hoc `debugfs write`, which lands the file's data
//!     blocks with mismatched checksums under post-hoc debugfs edits
//!     (1.47.0 on ubuntu-24.04 CI runners): the guest kernel then
//!     fails to `execve` the injected init with `EBADMSG` and panics
//!     ("Requested init … failed (error -74)") before it can be
//!     snapshotted. The bake path is the one `exec_real_vm` /
//!     `diff_snapshot` use, and it boots cleanly on every runner.
//!   - Snapshot once, destroy the source, rebuild memory.bin from its
//!     chunks (the residency step), then fresh-create **3** VMs from the
//!     same snapshot dir. All 3 mmap the SAME memory.bin inode.
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
//! Gating: Linux + KVM + firecracker + Docker.
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
use engram_core::types::ids::SandboxId;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig};
use engram_storage_local::LocalBlobStorage;
use tempfile::TempDir;

use common::fc_preflight;

const SIBLINGS: usize = 3;
const BLOB_MIB: u64 = 64;

/// The fill-complete marker lands on the serial console (`console=ttyS0` →
/// the jail's `firecracker.log`), so the host can poll for it instead of
/// sleeping a fixed window before snapshotting. It fires after the blob is
/// fully written — the pages the snapshot must capture are resident — and a
/// failed fill simply never emits it, surfacing as the poll's bounded
/// timeout + the RSS-floor assertion (the honest signal), same as before.
const FILL_DONE_MARKER: &str = "SPIKE-FILL-DONE";

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
/// at pack time (see the module docs) so the guest kernel can actually
/// `execve` it.
///
/// The read loop is `md5sum`, NOT `cat`: busybox `cat` copies via
/// `sendfile(2)`, and sendfile from tmpfs to /dev/null never dereferences
/// the page CONTENTS (the null driver discards the request without
/// reading), so a restored sibling would never re-fault the blob from
/// memory.bin and the RSS floor this test measures would collapse to the
/// boot set (strace-verified: busybox cat = 2 sendfile calls, 0 reads).
/// md5sum must pull every byte through userspace, faulting every page on
/// every pass regardless of the coreutils flavor.
const SPIKE_INIT: &str = "#!/bin/sh\n\
export PATH=/usr/sbin:/usr/bin:/sbin:/bin\n\
mount -t proc proc /proc 2>/dev/null || true\n\
mount -t devtmpfs dev /dev 2>/dev/null || true\n\
mount -t tmpfs -o size=128m tmpfs /tmp 2>/dev/null || true\n\
head -c 67108864 /dev/urandom > /tmp/blob 2>/dev/null || true\n\
echo SPIKE-FILL-DONE\n\
while true; do md5sum /tmp/blob > /dev/null 2>&1 || true; sleep 1; done\n";

/// Wait for the source guest's fill to complete (the console marker) —
/// the readiness poll that replaced a fixed 8 s post-create sleep.
async fn wait_for_fill(work: &Path, id: &impl std::fmt::Display) {
    let log = work.join(id.to_string()).join("firecracker.log");
    let contents =
        common::wait_for_log_contains(&log, &[FILL_DONE_MARKER], Duration::from_secs(30)).await;
    if !contents.contains(FILL_DONE_MARKER) {
        dump_fc_logs(work);
        panic!(
            "guest never reported {FILL_DONE_MARKER} within 30s — boot or blob fill failed \
             (see the dumped firecracker.log above for the guest console)"
        );
    }
}

/// Poll until every sibling has faulted its working set back in
/// (rss > blob) — the settle poll that replaced a fixed 5 s sleep. On
/// timeout we just fall through: the per-sibling RSS-floor assertions
/// that follow produce the diagnostic failure, exactly as before.
async fn settle_siblings(work: &Path, ids: &[impl std::fmt::Display]) {
    let _ = common::poll_until(Duration::from_secs(30), Duration::from_millis(250), || {
        ids.iter()
            .all(|id| smaps_rollup(fc_pid_for(work, &id.to_string())).rss_kb > BLOB_MIB * 1024)
    })
    .await;
}

/// ADR 0022 Option A: the *production* density path, end to end —
/// (a) materialize the per-template base memfile **from a chunk manifest**
/// (the residency step `image_prefetch` runs), and (b) fresh-create N
/// siblings off it (`restore_fresh`), asserting both page sharing and
/// per-sibling restore latency. The memory.bin every sibling maps is
/// **reconstructed from chunks**, proving the residency-materialized file
/// is byte-faithful AND shareable.
#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; bakes a rootfs and boots microVMs"]
async fn file_backend_base_create_shares_residency_memfile() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    let Some(busybox) = common::find_busybox() else {
        eprintln!("SKIP: no static busybox (apt install busybox-static or set BUSYBOX_STATIC)");
        return;
    };
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    let baked = bake_spike_rootfs(&busybox).await;

    let work = tempfile::tempdir().expect("work dir");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = None;
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
        swap_mib: None,
        swap_source: None,
        swap_manifest: None,
    };
    let source = SandboxId::new();
    backend.create(source, spec).await.expect("create");
    wait_for_fill(work.path(), &source).await;
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
    let cache = ChunkCache::new(ChunkCacheConfig {
        root: store_root.path().join("cache"),
        budget_bytes: 4 * 1024 * 1024 * 1024,
        sweep_debounce_ms: 0,
        eviction_enabled: true,
    });
    let chunk_store = ChunkStore::new(blob).with_chunk_cache(cache);
    let memory_manifest = chunk_store
        .chunk_file(&mem_path, ManifestKind::Memory, None)
        .await
        .expect("chunk memory.bin");

    std::fs::remove_file(&mem_path).expect("delete captured memory.bin");
    let mat = Instant::now();
    chunk_store
        .materialize_to_file_from(&memory_manifest, &mem_path, &chunk_store)
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
        let id = SandboxId::new();
        match backend
            .restore_fresh(
                id,
                metadata.clone(),
                common::disks(&baked.rootfs),
                Vec::new(),
            )
            .await
        {
            Ok(()) => {}
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
    settle_siblings(work.path(), &vms).await;

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

/// Bake the self-driving spike rootfs (init baked into the pack; see
/// the module docs for why post-hoc `debugfs write` is avoided). Returns
/// the ext4 rootfs path plus the tempdirs backing it — the caller must
/// keep `Baked` alive for as long as the rootfs is in use.
struct Baked {
    rootfs: PathBuf,
    _images: TempDir,
    _chunk_root: TempDir,
}

async fn bake_spike_rootfs(busybox: &Path) -> Baked {
    let images = tempfile::tempdir().expect("images dir");
    let chunk_root = tempfile::tempdir().expect("chunk store root");
    let blob: std::sync::Arc<dyn engram_core::traits::BlobStorage> =
        std::sync::Arc::new(LocalBlobStorage::new(chunk_root.path().to_path_buf()));
    let chunk_store = ChunkStore::new(blob);
    // No agentd/init injection: the guest is self-driving, booting the
    // COPY-equivalent `/spike-init.sh` directly (see `default_boot_args`).
    let outcome = common::bake_fixture_ext4(
        &images.path().join("rootfs.ext4"),
        &chunk_store,
        busybox,
        None,
        |tree| {
            use std::os::unix::fs::PermissionsExt;
            let init = tree.join("spike-init.sh");
            std::fs::write(&init, SPIKE_INIT)?;
            std::fs::set_permissions(&init, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        },
    )
    .await;
    Baked {
        rootfs: outcome.rootfs_path,
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
/// jail dir (the API socket path embeds the sandbox id).
fn fc_pid_for(work_dir: &Path, sandbox_id: &str) -> u32 {
    let needle = work_dir.join(sandbox_id).to_string_lossy().into_owned();
    for entry in std::fs::read_dir("/proc").expect("/proc").flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if String::from_utf8_lossy(&cmdline).contains(&needle) {
            return pid;
        }
    }
    panic!("no firecracker process found for sandbox {sandbox_id}");
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
