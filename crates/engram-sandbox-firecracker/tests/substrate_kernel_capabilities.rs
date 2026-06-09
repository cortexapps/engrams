//! ADR 0045 S0 kernel-capability gate for the unified memory substrate.
//!
//! The substrate (shared per-template base shm + UFFD `MINOR|WP` + per-sandbox
//! overlay with 4 KiB carve-on-first-write) rests on a handful of kernel
//! behaviors that are NOT guaranteed by any API contract we control:
//!
//!   - `MISSING|MINOR|WP` triple registration on a `MAP_SHARED` memfd mapping,
//!     with full-range `UFFDIO_WRITEPROTECT` arming (`wp_unpopulated`)
//!   - `UFFDIO_CONTINUE` with `MODE_WP` (a plain CONTINUE installs a WRITABLE
//!     pte and writes leak into the shared base — found by S0 probe v2)
//!   - write -> WP fault -> `MAP_FIXED` overlay carve -> `UFFDIO_WAKE` resumes
//!     the stalled writer; base stays clean, overlay holds the dirty page
//!   - KVM tolerates all of the above UNDER A LIVE MEMSLOT: guest reads fault
//!     as reads (no carve — sharing preserved), a guest write carves exactly
//!     one page mid-execution, and the KVM dirty log matches the carve set
//!
//! These were proven on engram-dev (kernel 6.8) and prod COS (6.12) on
//! 2026-06-09; this test keeps them proven on every CI runner kernel so a
//! kernel/KVM regression surfaces here, not in prod.
//!
//! The probes are small self-contained C programs (fixtures/substrate/) —
//! the exact artifacts the S0 spikes ran — compiled with the system gcc at
//! test time. Each prints `PASS`/`FAIL` per assertion and exits non-zero on
//! any failure; this test additionally greps for `FAIL` so a partial pass
//! can't slip through.
//!
//! `#[ignore]`d like the rest of the FC suite: needs Linux + (for the KVM
//! half) /dev/kvm. Wired into ci.yml's `test-firecracker` job.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::Command;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/substrate")
        .join(name)
}

/// gcc-compile a probe into the target tmpdir and run it; return stdout.
/// Panics (test failure) on compile error, non-zero exit, or any FAIL line.
fn run_probe(src_name: &str) -> String {
    let src = fixture(src_name);
    let out_dir = std::env::temp_dir().join(format!(
        "engram-substrate-probe-{}-{}",
        src_name.trim_end_matches(".c"),
        std::process::id()
    ));
    std::fs::create_dir_all(&out_dir).expect("create probe tmpdir");
    let bin = out_dir.join(src_name.trim_end_matches(".c"));

    let cc = Command::new("gcc")
        .arg("-O2")
        .arg("-pthread")
        .arg("-o")
        .arg(&bin)
        .arg(&src)
        .output()
        .expect("spawn gcc (required on FC test runners)");
    assert!(
        cc.status.success(),
        "gcc failed for {src_name}:\n{}",
        String::from_utf8_lossy(&cc.stderr)
    );

    let run = Command::new(&bin).output().expect("run probe");
    let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    println!("--- {src_name} stdout ---\n{stdout}");
    if !stderr.is_empty() {
        println!("--- {src_name} stderr ---\n{stderr}");
    }
    assert!(
        run.status.success(),
        "{src_name} exited non-zero ({:?})",
        run.status.code()
    );
    assert!(
        !stdout.contains("FAIL"),
        "{src_name} reported a FAIL line (see stdout above)"
    );
    let _ = std::fs::remove_dir_all(&out_dir);
    stdout
}

/// S0.1/S0.2: the userspace half — triple registration, WP arming,
/// CONTINUE|WP, the carve protocol, scattered-carve VMA behavior, install
/// and fault-round-trip throughput.
#[test]
#[ignore = "needs Linux uffd (MINOR|WP on shmem, kernel >= 5.19); run via test-firecracker CI job or dev-vm"]
fn substrate_uffd_capabilities() {
    let out = run_probe("uffd_probe.c");
    // The load-bearing assertions, by name — so a probe edit that drops one
    // shows up here instead of silently narrowing coverage.
    for marker in [
        "T1 PASS", // MISSING|MINOR|WP register + full-range WP arm
        "T2 PASS", // cached read -> MINOR -> CONTINUE|WP
        "T3 PASS", // hole read -> MISSING -> fill+CONTINUE|WP
        "T4 PASS", // write -> WP fault -> carve -> wake; base clean
        "T5 PASS", // unmapped-neighbor write still traps (no fault-around leak)
        "T6 PASS", // hole write traps and carves
    ] {
        assert!(out.contains(marker), "missing {marker:?} in probe output");
    }
}

/// S0.3/S0.5: the KVM half — a live guest on the substrate, reads don't
/// carve, a guest write carves under the live memslot, dirty log == carve set.
#[test]
#[ignore = "needs /dev/kvm; run via test-firecracker CI job or dev-vm"]
fn substrate_kvm_capabilities() {
    let out = run_probe("kvm_probe.c");
    assert!(
        out.contains("S0.3 PASS"),
        "missing S0.3 PASS in probe output"
    );
    assert!(
        out.contains("only the written page: yes"),
        "guest READS carved — KVM faulted reads as writes; sharing would be lost"
    );
}
