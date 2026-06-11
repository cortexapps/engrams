//! ADR 0045 S0 kernel-capability gate for the unified memory substrate.
//!
//! The substrate — **v2b, the design of record after the S0.6 cross-process
//! probe**: guest RAM = `MAP_PRIVATE` of the per-template base shm file,
//! registered UFFD `MISSING|MINOR` — rests on kernel behaviors NOT
//! guaranteed by any API contract we control:
//!
//!   - `MISSING|MINOR` registration AND cross-process `UFFDIO_CONTINUE` are
//!     legal on a `MAP_PRIVATE` mapping of a shm file (base-identical reads
//!     resolve against the shared page cache — the density mechanism)
//!   - `UFFDIO_COPY` on that same mapping installs session-divergent pages
//!     PRIVATELY (the shared base file stays clean)
//!   - guest writes COW natively (privacy with zero handler involvement)
//!   - KVM tolerates all of the above UNDER A LIVE MEMSLOT, and its dirty
//!     log catches the COW writes (teardown keeps the diff chain)
//!
//! The first-round WP/overlay-carve protocol (uffd_probe.c) is kept gated as
//! the documented fallback — superseded because its `mmap(MAP_FIXED)` carve
//! cannot cross the FC/handler process boundary.
//!
//! All proven on engram-dev (kernel 6.8) and prod COS (6.12) on 2026-06-09;
//! this test keeps them proven on every CI runner kernel so a kernel/KVM
//! regression surfaces here, not in prod.
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

/// S0.6 — the v2b design of record: MISSING|MINOR registration and
/// cross-process CONTINUE on a MAP_PRIVATE-of-shm-file mapping; native COW
/// for write privacy; UFFDIO_COPY installing session-divergent pages
/// privately; sibling page-cache sharing (the density mechanism).
/// ADR 0045 Phase C2 gate-opener: the pagemap-anon dirty map + the
/// host-agent-as-page-server read path. Proves, from the parent's seat
/// (= the host-agent's, FC's parent, so YAMA permits readv):
/// COPY-installed and COW pages classify as anon, clean CONTINUE pages
/// as file-backed, untouched as absent; process_vm_readv returns
/// correct bytes for all classes, including faulting an untouched page
/// through the OWNER's handler. A FAIL here means C2 falls back to a
/// KVM-dirty-bitmap fork surface.
#[test]
#[ignore = "Linux probe; runs in CI's test-firecracker job"]
fn substrate_pagemap_dirty_map() {
    let out = run_probe("pagemap_probe.c");
    for marker in [
        "PASS T1a", // COPY-installed => present+anon
        "PASS T1b", // COW-written => present+anon
        "PASS T1c", // clean CONTINUE => present+file
        "PASS T1d", // untouched => not present
        "PASS T2a", // readv: COPY bytes
        "PASS T2b", // readv: COW bytes
        "PASS T2c", // readv: base bytes
        "PASS T3a", // readv faults through the owner's handler
        "PASS T3b", // post-readv reclassifies to file
    ] {
        assert!(out.contains(marker), "missing {marker} in:\n{out}");
    }
}

#[test]
#[ignore = "needs Linux uffd (MINOR on shmem, kernel >= 5.13); run via test-firecracker CI job or dev-vm"]
fn substrate_cross_process_map_private() {
    let out = run_probe("cross_probe.c");
    for marker in [
        "X1 PASS", // MISSING|MINOR register on MAP_PRIVATE shm
        "X2 PASS", // cross-process CONTINUE resolves a read
        "X3 PASS", // cross-process hole fill (pwrite-by-path + CONTINUE)
        "X4 PASS", // native COW: private write, file clean
        "X5 PASS", // sibling mapping shares the clean base
        "X6 PASS", // UFFDIO_COPY installs divergent pages privately
    ] {
        assert!(out.contains(marker), "missing {marker:?} in probe output");
    }
}

/// S0.3/S0.5 (v2b): a live KVM guest on the substrate — base reads CONTINUE
/// (shared), the divergent page COPYs (exactly one), guest writes COW
/// natively with the base file staying clean, and the KVM dirty log catches
/// the write.
#[test]
#[ignore = "needs /dev/kvm; run via test-firecracker CI job or dev-vm"]
fn substrate_kvm_capabilities() {
    let out = run_probe("kvm_probe.c");
    assert!(
        out.contains("S0.3 PASS"),
        "missing S0.3 PASS in probe output"
    );
    assert!(
        out.contains("S0.5 PASS"),
        "missing S0.5 PASS in probe output"
    );
    assert!(
        out.contains("only the divergent page: yes"),
        "base reads triggered COPYs — sharing would be lost"
    );
}

/// The superseded WP/carve protocol (the S0.1/S0.2 first-round probe) —
/// kept as the documented fallback: MINOR|WP triple registration, upfront
/// WP arming (suppresses fault-around), CONTINUE|MODE_WP, and the overlay
/// carve. Not the design of record (the carve's mmap(MAP_FIXED) can't cross
/// the FC/handler process boundary), but the kernel behaviors stay gated.
#[test]
#[ignore = "needs Linux uffd (MINOR|WP on shmem, kernel >= 5.19); run via test-firecracker CI job or dev-vm"]
fn substrate_uffd_carve_fallback() {
    let out = run_probe("uffd_probe.c");
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
