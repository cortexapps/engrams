//! ADR 0068 / issue #531 (PR #564 review, incident fbd3794c):
//! `FirecrackerBackend::probe_sandbox`'s map-vs-process DESYNC arm.
//!
//! `probe_sandbox` exists precisely because the in-memory `self.sandboxes`
//! map (what `list()`/`running_sandboxes` reflect) can lie: a fresh
//! host-agent generation that hasn't reattached yet knows about NO
//! sandboxes, even though every VM from the previous generation is still
//! running (ADR 0044 K2 detach). `reconcile::flip_missing` uses this probe
//! as an independent ground-truth check before declaring a session's
//! sandbox gone — see `engram_core::types::sandbox::SandboxProbe`'s doc
//! comment. This test pins the two outcomes that matter:
//!
//!   1. `known_to_backend == false` (the backend's map has no entry — the
//!      exact shape of a not-yet-reattached generation) while the
//!      process the on-disk manifest points to is genuinely alive →
//!      `process_alive == true`. This is the desync `flip_missing` must
//!      NOT treat as "sandbox gone".
//!   2. Once the manifest is gone too (mirrors what `destroy()` does
//!      before this crate's teardown completes), the probe returns the
//!      honest negative: `known_to_backend == false`, `process_alive ==
//!      false`.
//!
//! ## Why this doesn't need a real Firecracker VM
//!
//! `probe_sandbox`'s implementation (`src/lib.rs`) touches exactly two
//! things: the in-memory `DashMap` (`known_to_backend`), and the on-disk
//! `sandbox_manifest` + a `/proc/<pid>` read (`process_alive`) — no
//! jailer, no KVM ioctl, no `firecracker` binary. So the cheapest honest
//! construction is a `FirecrackerBackend` that never `create()`d this
//! sandbox at all (map has no entry — stronger than "a fresh generation
//! that hasn't reattached", since even reattach never ran) plus a
//! hand-written manifest pointing at THIS TEST PROCESS's own real pid —
//! genuinely alive for the test's duration, with real
//! `start_time_jiffies`/`comm` read the same way the reattach pass does.
//! `#[ignore]`'d (Linux-only: reads `/proc`) and wired into the
//! `tests (linux)` CI lane — unlike its FC-crate siblings it needs
//! neither `/dev/kvm` nor the `firecracker` binary, so it runs on every
//! Rust-touching PR, not just the FC dep closure (ADR 0098:
//! keep-the-FC-lane-minimal).

#![cfg(target_os = "linux")]

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_core::SandboxId;
use engram_sandbox_firecracker::sandbox_manifest::{
    self, FirecrackerProcessRecord, ProcessRecord, SandboxManifest,
};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig};

fn test_spec() -> SandboxSpec {
    SandboxSpec {
        image: "fc-probe-desync-test".into(),
        rootfs_source: None,
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 64 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: Default::default(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
        swap_mib: None,
    }
}

/// Write a manifest at `work_dir/<id>/sandbox.json` whose `firecracker`
/// process record is THIS test process's own real pid/jiffies/comm —
/// genuinely alive for as long as the test runs, verifiable via the
/// exact `/proc` reads `probe_sandbox` itself uses.
fn write_manifest_pointing_at_self(work_dir: &std::path::Path, id: SandboxId) {
    write_manifest_pointing_at(work_dir, id, std::process::id());
}

/// Same, for an arbitrary live (or zombie — `/proc` stays readable
/// until the parent reaps) pid.
fn write_manifest_pointing_at(work_dir: &std::path::Path, id: SandboxId, pid: u32) {
    let start_time_jiffies = sandbox_manifest::read_proc_start_time_jiffies(pid)
        .expect("must be able to read the pid's /proc stat on Linux");
    let comm = sandbox_manifest::read_proc_comm(pid)
        .expect("must be able to read the pid's /proc comm on Linux");
    let manifest = SandboxManifest {
        schema_version: sandbox_manifest::SCHEMA_VERSION,
        sandbox_id: id,
        backend: sandbox_manifest::BACKEND_FIRECRACKER.to_string(),
        spec: test_spec(),
        firecracker: FirecrackerProcessRecord {
            process: ProcessRecord {
                pid,
                start_time_jiffies,
                comm,
            },
            api_socket: work_dir.join("fc.sock"),
            vsock_uds_base: work_dir.join("sb.vsock"),
            rootfs_canonical: work_dir.join("rootfs.dev"),
            swap_canonical: None,
            vsock_cid: 3,
        },
        network: None,
        netns: None,
        uffd_handler: None,
        migration_role: None,
    };
    sandbox_manifest::write_manifest(&sandbox_manifest::manifest_path(work_dir, id), &manifest)
        .expect("write manifest");
}

/// The fbd3794c incident shape: `known_to_backend == false` (this
/// `FirecrackerBackend` never `create()`d or reattached this sandbox — no
/// map entry at all) while the manifest's recorded process is genuinely
/// alive. `reconcile::flip_missing` must be able to tell this apart from
/// "actually gone" — that's the whole reason `probe_sandbox` overrides
/// the trait default instead of deriving from the same in-memory map
/// `list()` already gets wrong.
#[tokio::test]
#[ignore = "Linux-only (reads /proc); run with --ignored"]
async fn probe_sandbox_reports_alive_process_unknown_to_backend() {
    let work = tempfile::tempdir().expect("tempdir");
    let backend = FirecrackerBackend::new(
        work.path(),
        FirecrackerConfig::with_kernel("/nonexistent/kernel"),
    );
    let id = SandboxId::new();
    write_manifest_pointing_at_self(work.path(), id);

    let probe = backend
        .probe_sandbox(id)
        .await
        .expect("probe_sandbox must not error on a manifest-only sandbox");
    assert!(
        !probe.known_to_backend,
        "this backend never created/reattached the sandbox — its map must have no entry"
    );
    assert!(
        probe.process_alive,
        "the manifest's recorded process (this test process) is genuinely alive"
    );
}

/// Once the manifest is gone too — mirroring what `destroy()` does to
/// the manifest before this crate's teardown work completes — the probe
/// has nothing to independently verify against and falls back to
/// `known_to_backend` (still false here), giving the honest negative on
/// both axes rather than a stale "maybe alive".
#[tokio::test]
#[ignore = "Linux-only (reads /proc); run with --ignored"]
async fn probe_sandbox_after_manifest_removed_is_the_honest_negative() {
    let work = tempfile::tempdir().expect("tempdir");
    let backend = FirecrackerBackend::new(
        work.path(),
        FirecrackerConfig::with_kernel("/nonexistent/kernel"),
    );
    let id = SandboxId::new();
    write_manifest_pointing_at_self(work.path(), id);

    // Sanity: the probe sees the alive process before we remove anything
    // (same assertion as the sibling test — pins the precondition this
    // test's "after" is relative to).
    let before = backend.probe_sandbox(id).await.expect("probe before");
    assert!(before.process_alive);

    // Mirrors `destroy()`'s manifest delete — the recorded process may
    // even still be alive at this instant in a real teardown race, but
    // with no manifest to verify against, the probe can't tell, and
    // falls back to `known_to_backend` rather than guessing "alive".
    sandbox_manifest::delete_manifest(&sandbox_manifest::manifest_path(work.path(), id));

    let after = backend
        .probe_sandbox(id)
        .await
        .expect("probe_sandbox must not error on a missing manifest");
    assert!(
        !after.known_to_backend,
        "still no map entry — this backend never created/reattached the sandbox"
    );
    assert!(
        !after.process_alive,
        "no manifest to verify against falls back to known_to_backend (false) — \
         the honest negative, not a stale 'maybe alive'"
    );
}

/// Issue #1012 regression: a killed-but-unreaped child (its parent holds
/// the un-`wait()`ed `Child`, as `LiveSandbox` does) keeps `/proc/<pid>`
/// readable, so the three-axis identity check STILL MATCHES — but the
/// process is a zombie and can never run again. `process_alive` must be
/// false, or every probe consumer (heartbeat reconcile, the #777
/// straggler defer, ADR 0068 `flip_missing` rescue) defers forever to a
/// ghost VM.
#[tokio::test]
#[ignore = "Linux-only (reads /proc); run with --ignored"]
async fn probe_sandbox_reports_zombie_with_matching_identity_as_dead() {
    let work = tempfile::tempdir().expect("tempdir");
    let backend = FirecrackerBackend::new(
        work.path(),
        FirecrackerConfig::with_kernel("/nonexistent/kernel"),
    );
    let id = SandboxId::new();

    // A real child whose identity we record while it is alive…
    let mut child = tokio::process::Command::new("sleep")
        .arg("600")
        .spawn()
        .expect("spawn sleep");
    let pid = child.id().expect("child has pid");
    write_manifest_pointing_at(work.path(), id, pid);

    // …then SIGKILL it WITHOUT `wait()`ing: `child` stays held, so the
    // pid is a zombie of this test process — identity axes still match.
    // SAFETY: SIGKILL on a child pid we just spawned.
    let rc = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    assert_eq!(rc, 0, "SIGKILL the sleep child");
    // The kernel flips the state to Z asynchronously; wait for it so the
    // test asserts the probe's classification, not the kill's latency.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while sandbox_manifest::read_proc_state(pid) != Some('Z') {
        assert!(
            std::time::Instant::now() < deadline,
            "child never reached zombie state"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let probe = backend.probe_sandbox(id).await.expect("probe zombie");
    // Reap before asserting so a failure doesn't leak the zombie.
    let _ = child.wait().await;
    assert!(
        !probe.process_alive,
        "a zombie with a matching three-axis identity must probe dead (issue #1012)"
    );
}
