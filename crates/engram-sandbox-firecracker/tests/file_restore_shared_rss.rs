//! ADR 0022 Option A go/no-go spike + regression gate: **N File-backend
//! restores off one memory.bin share clean pages via the host page
//! cache** (`MAP_PRIVATE` of one inode → one physical copy per page).
//!
//! This is the density thesis in miniature:
//!
//!   - Bake nothing: copy the cached Ubuntu test rootfs and inject (via
//!     `debugfs`, no mount/root needed) an init that fills a 64 MiB
//!     tmpfs blob and re-reads it every second — a guest working set
//!     that every sibling re-touches after restore, so the shared pages
//!     actually fault in and become measurable.
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
//! Gating: Linux + KVM + firecracker + `debugfs` (e2fsprogs). Run:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-sandbox-firecracker --test file_restore_shared_rss -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig};

use common::{fc_preflight, require_bin};

const SIBLINGS: usize = 3;
const BLOB_MIB: u64 = 64;

/// Init injected into the rootfs: build the blob in tmpfs (guest RAM),
/// then re-read it forever so restored siblings keep faulting the same
/// guest-physical pages back in from memory.bin.
const SPIKE_INIT: &str = r#"#!/bin/bash
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
# The minimal FC-CI Ubuntu rootfs has no /mnt and no /dev/shm; /tmp is
# the one guaranteed tmpfs-able mountpoint (verified via serial console
# on the dev-vm — "mount point does not exist" otherwise). devtmpfs is
# kernel-auto-mounted, so /dev/urandom is available. No `|| true`: if
# the mount fails the blob write fails too and the host-side RSS-floor
# assertion catches it loudly.
mount -t tmpfs -o size=128m tmpfs /tmp
head -c 67108864 /dev/urandom > /tmp/blob
while true; do cat /tmp/blob > /dev/null; sleep 1; done
"#;

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + debugfs; boots microVMs"]
async fn file_backend_siblings_share_clean_pages() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_bin("debugfs") {
        return;
    }

    // ---- 1. Copy the cached rootfs (never mutate the cache) + inject init ----
    let work = tempfile::tempdir().expect("work dir");
    let rootfs = work.path().join("rootfs.ext4");
    std::fs::copy(&env.rootfs, &rootfs).expect("copy test rootfs");
    let init = work.path().join("spike-init.sh");
    std::fs::write(&init, SPIKE_INIT).expect("write init");
    debugfs(&rootfs, &format!("write {} /spike-init.sh", init.display()));
    debugfs(&rootfs, "sif /spike-init.sh mode 0100755");

    // ---- 2. Boot, let the blob fill, snapshot, destroy ----
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = None; // unprivileged test — see lifecycle.rs
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/spike-init.sh".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "fc-shared-rss-test".into(),
        rootfs_source: Some(rootfs),
        image_uri: None,
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
    // Boot + 64 MiB urandom fill + at least one read pass.
    tokio::time::sleep(Duration::from_secs(8)).await;
    let metadata = backend.snapshot(source).await.expect("snapshot");
    backend.destroy(source).await.expect("destroy source");

    // ---- 3. Restore N siblings off the same memory.bin ----
    // Serial restores (the per-snapshot canonical vsock UDS path is
    // re-bound by each load — last binder owns it; that only breaks
    // host→guest exec, which this test doesn't use). All siblings stay
    // alive together: that's the sharing condition.
    let mut vms = Vec::new();
    for i in 0..SIBLINGS {
        let t = Instant::now();
        let id = backend
            .restore(metadata.clone())
            .await
            .unwrap_or_else(|e| panic!("restore sibling {i}: {e:?}"));
        eprintln!(
            "SPIKE: restore sibling {i} took {} ms",
            t.elapsed().as_millis()
        );
        vms.push(id);
    }

    // Let each sibling's read loop sweep the blob a few times so the
    // common working set is faulted in everywhere.
    tokio::time::sleep(Duration::from_secs(5)).await;

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

/// Run one `debugfs -w -R <cmd>` against `img`, panicking on failure.
/// (`debugfs` exits 0 even on some errors; stderr containing
/// "File not found"/"Operation not permitted" is the real signal.)
fn debugfs(img: &Path, cmd: &str) {
    let out = std::process::Command::new("debugfs")
        .arg("-w")
        .arg("-R")
        .arg(cmd)
        .arg(img)
        .output()
        .expect("spawn debugfs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success()
            && !stderr.contains("File not found")
            && !stderr.contains("Operation not permitted"),
        "debugfs -R {cmd:?} failed: {stderr}",
    );
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
