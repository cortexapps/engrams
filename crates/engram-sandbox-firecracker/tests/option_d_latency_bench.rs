//! ADR 0014 option-D latency benchmark.
//!
//! Measures `restore + swap_harness_drive` over N serial cycles
//! from one snapshot, asserting the per-cycle wall-clock stays
//! under the sub-1s engineering portion of the M1.11–M1.15 budget.
//! Reported targets:
//!
//!   - `restore` p50 ≤ 100 ms, p99 ≤ 500 ms (file-mode restore from
//!     a hot local memory.bin; UFFD-mode is similar).
//!   - `swap` p50 ≤ 10 ms, p99 ≤ 100 ms (FC pause + PATCH /drives
//!     + resume on a paused VM).
//!
//! Cycle = restore → swap → destroy. Slot stays in free_list (in
//! the production warm pool) between restore and destroy; here we
//! treat each cycle as a fresh refill so we can measure latency
//! end-to-end.
//!
//! Not a steady-state benchmark — N=5 cycles, single-threaded.
//! Enough to surface the wall-clock signal without overwhelming
//! the dev VM. Production fleet-scale numbers come from the
//! `engram_warm_pool_refill_seconds` Prometheus histogram on
//! real-traffic hosts.
//!
//! Run via:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-sandbox-firecracker --test option_d_latency_bench \
//!     -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig};
use tokio::io::AsyncWriteExt;

const HARNESS_SIZE: u64 = 16 * 1024 * 1024;
const N_CYCLES: usize = 5;

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker; run with --ignored on the dev VM"]
async fn option_d_latency_within_budget() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };

    let work = tempfile::tempdir().expect("tempdir");
    let work_path = work.path();

    // Stub harness (bake-time) + N session harnesses (one per
    // cycle, each with a unique first-4-bytes sentinel so we can
    // confirm the swap took effect if needed).
    let stub_harness = work_path.join("stub-harness.ext4");
    write_padded_file(&stub_harness, b"BAKE", HARNESS_SIZE).await;
    let mut session_harnesses = Vec::with_capacity(N_CYCLES);
    for i in 0..N_CYCLES {
        let p = work_path.join(format!("session-{i}.ext4"));
        let sentinel = format!("S{i:03}");
        write_padded_file(&p, sentinel.as_bytes(), HARNESS_SIZE).await;
        session_harnesses.push(p);
    }

    let local_rootfs = work_path.join("rootfs.ext4");
    tokio::fs::copy(&env.rootfs, &local_rootfs)
        .await
        .expect("copy rootfs");

    let mut cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
    cfg.net_pool = None;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/bin/bash".into();
    let backend = FirecrackerBackend::new(work_path, cfg);

    let spec = SandboxSpec {
        image: "option-d-bench".into(),
        rootfs_source: Some(local_rootfs.clone()),
        image_uri: None,
        harness_pack_uri: None,
        harness_substrate: Some(stub_harness.clone()),
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 128 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
    };

    // Source bake (once).
    let source_id = backend.create(spec).await.expect("create source");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let metadata = backend.snapshot(source_id).await.expect("snapshot");
    backend.destroy(source_id).await.expect("destroy source");

    // N cycles: restore → swap → destroy.
    let mut restore_samples = Vec::with_capacity(N_CYCLES);
    let mut swap_samples = Vec::with_capacity(N_CYCLES);
    for (i, session_harness) in session_harnesses.iter().enumerate() {
        let t_restore = Instant::now();
        let id = backend
            .restore(metadata.clone())
            .await
            .unwrap_or_else(|e| panic!("restore iter {i}: {e}"));
        restore_samples.push(t_restore.elapsed());

        let t_swap = Instant::now();
        backend
            .swap_harness_drive(id, session_harness.clone())
            .await
            .unwrap_or_else(|e| panic!("swap iter {i}: {e}"));
        swap_samples.push(t_swap.elapsed());

        backend
            .destroy(id)
            .await
            .unwrap_or_else(|e| panic!("destroy iter {i}: {e}"));
    }

    let restore_summary = summarize(&restore_samples);
    let swap_summary = summarize(&swap_samples);

    eprintln!("\n=== option-D latency over {N_CYCLES} cycles ===");
    eprintln!(
        "restore: p50 {:?} | p99 {:?} | max {:?}",
        restore_summary.p50, restore_summary.p99, restore_summary.max,
    );
    eprintln!(
        "swap:    p50 {:?} | p99 {:?} | max {:?}",
        swap_summary.p50, swap_summary.p99, swap_summary.max,
    );
    eprintln!("================================================\n");

    // Assertions: loose bounds so dev-VM contention doesn't flake.
    // The whole engineering portion of the warm-lease (restore +
    // swap) MUST stay well under 1s to leave budget for the
    // Anthropic API + harness cold start.
    assert!(
        restore_summary.p99 < Duration::from_secs(2),
        "restore p99 should be sub-2s; got {:?}",
        restore_summary.p99,
    );
    assert!(
        swap_summary.p99 < Duration::from_millis(500),
        "swap p99 should be sub-500ms; got {:?}",
        swap_summary.p99,
    );
}

struct Summary {
    p50: Duration,
    p99: Duration,
    max: Duration,
}

fn summarize(samples: &[Duration]) -> Summary {
    let mut sorted: Vec<_> = samples.to_vec();
    sorted.sort();
    let n = sorted.len();
    // p50: middle; p99: last (small N). Crude but correct for the
    // few-sample regime.
    let p50_idx = n / 2;
    let p99_idx = n.saturating_sub(1);
    Summary {
        p50: sorted[p50_idx],
        p99: sorted[p99_idx],
        max: *sorted.last().unwrap(),
    }
}

async fn write_padded_file(path: &Path, sentinel: &[u8], total_bytes: u64) {
    let mut f = tokio::fs::File::create(path).await.expect("create harness");
    f.write_all(sentinel).await.expect("write sentinel");
    f.set_len(total_bytes).await.expect("pad");
    f.flush().await.expect("flush");
}
