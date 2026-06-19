//! ADR 0027 → ADR 0035 aux read-only drive mechanism tests.
//!
//! The RO-mount skills/browser engine attaches a fleet-wide bundle
//! (skills / playwright squashfs) as an *additional* read-only
//! virtio-blk drive. ADR 0027 shipped this with a fixed fleet-canonical
//! path and "re-anchor by presence"; the 2026-06-03 incident proved
//! that contract unsound — a host roll swapped the bytes under the
//! fixed path while live base snapshots still re-anchored against it,
//! and every bundle read in the guest EIO'd (frozen squashfs
//! superblock ↔ different backing bytes). An earlier revision of THIS
//! file pinned that behavior as a feature ("symlink roll serves new
//! bytes"); ADR 0035 inverts it. Three properties, one per test:
//!
//!   - `aux_ro_drive_reopens_pinned_generation`: the snapshot embeds a
//!     content-addressed path (`skills-<sha>.squashfs`); a newer
//!     generation appearing alongside changes nothing — restore
//!     reopens the pinned file and the guest reads the ORIGINAL bytes.
//!     (Resume flavor: an in-flight session keeps its world.)
//!   - `aux_ro_drive_content_swap_under_snapshot_is_the_incident`: the
//!     negative control. Mutate the bytes at the SAME path between
//!     snapshot and restore and the guest silently reads the NEW bytes
//!     through its capture-time device state — which at the
//!     filesystem layer is the EIO corruption. This is exactly what a
//!     pre-0035 host roll did; content-addressed filenames make it
//!     unconstructable in production (nothing ever writes to an
//!     existing `<name>-<sha>` path).
//!   - `aux_ro_drive_patch_then_resume_serves_new_generation`: the
//!     fresh-create flavor (ADR 0035 §3): load PAUSED, `patch_drive`
//!     the aux drive to a newer generation, resume — the guest reads
//!     the NEW bytes. (The in-guest squashfs re-mount half of §3 lives
//!     in agentd; the stock FC test kernel has no squashfs, so these
//!     tests pin the block-device layer and the dev-vm e2e covers the
//!     mount layer.)
//!
//! Same gating as the other ignored FC integration tests (Linux + KVM +
//! firecracker on PATH + fetched test artifacts), plus mount(8) for the
//! init-script injection.
//!
//! Run via:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-sandbox-firecracker --test aux_ro_drive \
//!     -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use engram_sandbox_firecracker::client::{
    ActionType, BootSource, DriveConfig, FirecrackerClient, MachineConfig, VmState,
};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

const SENTINEL_A: &[u8] = b"AAAA";
const SENTINEL_B: &[u8] = b"BBBB";

/// 16 MiB — same rationale as `patch_drive_swap`: a clean virtio-blk
/// geometry log and no "capacity change from 0" quirk.
const BUNDLE_SIZE: u64 = 16 * 1024 * 1024;

/// What happens to the aux drive between snapshot and restore.
enum Mutation {
    /// Nothing: a newer generation lands ALONGSIDE the pinned file
    /// (content-addressed roll); the pinned path is untouched.
    NewGenerationAlongside,
    /// The incident: the bytes at the embedded path are replaced
    /// in-place (what a pre-0035 host roll effectively did).
    SwapContentInPlace,
    /// ADR 0035 §3 fresh-create flavor: load paused, `patch_drive`
    /// to the new generation's file, then resume.
    PatchToNewGeneration,
}

/// Resume flavor (ADR 0035 §3): the pinned generation is what the
/// guest keeps reading, no matter what newer generations exist.
#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + sudo mount; run with --ignored on the dev VM"]
async fn aux_ro_drive_reopens_pinned_generation() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !common::require_bin("mount") || !common::require_bin("umount") {
        return;
    }
    run_scenario(&env, Mutation::NewGenerationAlongside, SENTINEL_A).await;
}

/// Negative control — the incident, reproduced. FC reopens the
/// embedded path at `load_snapshot` and happily serves whatever bytes
/// are there now; the guest's capture-time view of the device is
/// silently violated. If this test ever needs "fixing" to expect the
/// OLD bytes, something upstream started pinning content for us; if a
/// production path ever recreates this shape (mutating an existing
/// staged file), THIS is the corruption it causes.
#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + sudo mount; run with --ignored on the dev VM"]
async fn aux_ro_drive_content_swap_under_snapshot_is_the_incident() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !common::require_bin("mount") || !common::require_bin("umount") {
        return;
    }
    run_scenario(&env, Mutation::SwapContentInPlace, SENTINEL_B).await;
}

/// Fresh-create flavor (ADR 0035 §3): the deliberate, paused-window
/// swap to the host's current generation. The proven option-D
/// mechanism (`patch_drive_swap`) applied to the bundle drive.
#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + sudo mount; run with --ignored on the dev VM"]
async fn aux_ro_drive_patch_then_resume_serves_new_generation() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !common::require_bin("mount") || !common::require_bin("umount") {
        return;
    }
    run_scenario(&env, Mutation::PatchToNewGeneration, SENTINEL_B).await;
}

/// ADR 0055 device-ceiling probe. Boot + snapshot + restore a VM with
/// `AuxRoDrive::RESERVED_SLOTS` reserved aux RO drives (`dyn-0..dyn-{N-1}`) —
/// the per-image base-snapshot device model. Confirms Firecracker accepts N aux
/// virtio-blk drives on the x86 virtio-mmio legacy-GSI pool (engrams boots
/// `pci=off`, `GSI_LEGACY_START=5..GSI_LEGACY_END=23`, so ~13 lines minus
/// rootfs/net/vsock) and that the snapshot/restore path carries all N. If the
/// pool can't fit N, `InstanceStart` / `load_snapshot` / the guest boot fails
/// HERE — which is exactly the ceiling the ADR's measurement gate names: drop
/// `RESERVED_SLOTS` until this passes. (No net/vsock here, so this probe has
/// MORE GSI headroom than production; treat a pass as necessary-not-sufficient
/// and keep headroom in `RESERVED_SLOTS`.)
#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + sudo mount; run with --ignored on the dev VM"]
async fn reserved_slots_n_drives_boot_snapshot_restore() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !common::require_bin("mount") || !common::require_bin("umount") {
        return;
    }
    let n = engram_core::types::sandbox::AuxRoDrive::RESERVED_SLOTS;

    let work = tempfile::tempdir().expect("tempdir");
    let work = work.path();

    // N aux-drive files. Content is irrelevant to the ceiling probe — the
    // guest only reads /dev/vdb (= dyn-0) post-resume as a liveness check.
    let mut aux_files = Vec::with_capacity(n);
    for i in 0..n {
        let f = work.join(format!("dyn-{i}.bin"));
        write_padded_file(&f, SENTINEL_A, BUNDLE_SIZE).await;
        aux_files.push(f);
    }

    let rootfs = work.join("rootfs.ext4");
    tokio::fs::copy(&env.rootfs, &rootfs)
        .await
        .expect("copy rootfs");
    install_init_script(&rootfs, work).await;

    let fc1_log = work.join("fc1.log");
    let fc1_api = work.join("fc1.sock");
    let mut fc1 = spawn_firecracker(&fc1_api, &fc1_log).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let client1 = FirecrackerClient::new(&fc1_api);
    configure_boot_n(&client1, &env.kernel, &rootfs, &aux_files).await;
    client1
        .put_action(ActionType::InstanceStart)
        .await
        .unwrap_or_else(|e| {
            panic!("InstanceStart with {n} aux drives failed (FC device ceiling?): {e}")
        });

    // Boot + enter the 12s snapshot-window sleep.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let snapshot_paths = client1
        .create_snapshot(work)
        .await
        .unwrap_or_else(|e| panic!("create_snapshot with {n} aux drives failed: {e}"));
    let _ = fc1.start_kill();
    let _ = fc1.wait().await;

    let fc2_log = work.join("fc2.log");
    let fc2_api = work.join("fc2.sock");
    let mut fc2 = spawn_firecracker(&fc2_api, &fc2_log).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let client2 = FirecrackerClient::new(&fc2_api);
    client2
        .load_snapshot_paused(&snapshot_paths)
        .await
        .unwrap_or_else(|e| panic!("load_snapshot with {n} aux drives failed: {e}"));
    client2
        .patch_vm_state(VmState::Resumed)
        .await
        .expect("resume");

    let post_log = wait_for_post_resume_reads(&fc2_log, 5, Duration::from_secs(40)).await;
    let hits = post_log
        .lines()
        .filter(|l| l.contains("post_resume_iter"))
        .count();
    assert!(
        hits >= 5,
        "guest didn't complete post-resume reads with {n} aux drives \
         (boot/restore broke at the ceiling?); got {hits}:\n{post_log}",
    );

    let _ = fc2.start_kill();
    let _ = fc2.wait().await;
}

async fn run_scenario(env: &common::FcEnv, mutation: Mutation, expected_post_resume: &[u8]) {
    let work = tempfile::tempdir().expect("tempdir");
    let work = work.path();

    // ── content-addressed generation files (production shape) ──
    //
    // `gen_a` is the pinned generation the snapshot embeds; `gen_b`
    // is the newer generation a roll stages alongside it.
    let gen_a = work.join(format!("skills-{}.squashfs", "a".repeat(64)));
    write_padded_file(&gen_a, SENTINEL_A, BUNDLE_SIZE).await;
    let gen_b = work.join(format!("skills-{}.squashfs", "b".repeat(64)));
    write_padded_file(&gen_b, SENTINEL_B, BUNDLE_SIZE).await;

    // ── prepare rootfs copy with init.experiment ──
    let rootfs = work.join("rootfs.ext4");
    tokio::fs::copy(&env.rootfs, &rootfs)
        .await
        .expect("copy rootfs");
    install_init_script(&rootfs, work).await;

    // ── FC #1: boot with the pinned generation attached, snapshot ──
    let fc1_log = work.join("fc1.log");
    let fc1_api = work.join("fc1.sock");
    let mut fc1 = spawn_firecracker(&fc1_api, &fc1_log).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client1 = FirecrackerClient::new(&fc1_api);
    configure_boot(&client1, &env.kernel, &rootfs, &gen_a).await;
    client1
        .put_action(ActionType::InstanceStart)
        .await
        .expect("instance start");

    // Give the guest time to boot + enter the 12s snapshot-window sleep.
    tokio::time::sleep(Duration::from_secs(6)).await;

    let snapshot_paths = client1
        .create_snapshot(work)
        .await
        .expect("create snapshot");
    let _ = fc1.start_kill();
    let _ = fc1.wait().await;

    // ── the between-snapshot-and-restore mutation under test ──
    if matches!(mutation, Mutation::SwapContentInPlace) {
        // The incident: same path, different bytes. (Production can no
        // longer construct this — generations are immutable files —
        // but the test pins what FC does if anything ever regresses.)
        write_padded_file(&gen_a, SENTINEL_B, BUNDLE_SIZE).await;
    }

    // ── FC #2: load paused, optionally patch, resume ──
    let fc2_log = work.join("fc2.log");
    let fc2_api = work.join("fc2.sock");
    let mut fc2 = spawn_firecracker(&fc2_api, &fc2_log).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let client2 = FirecrackerClient::new(&fc2_api);

    client2
        .load_snapshot_paused(&snapshot_paths)
        .await
        .expect("load_snapshot_paused");

    if matches!(mutation, Mutation::PatchToNewGeneration) {
        // ADR 0035 §3: the fresh-create swap happens in the paused
        // window, exactly like the production restore path.
        client2
            .patch_drive("skills", &gen_b)
            .await
            .expect("patch_drive to new generation");
    }

    client2
        .patch_vm_state(VmState::Resumed)
        .await
        .expect("resume");

    let post_log = wait_for_post_resume_reads(&fc2_log, 5, Duration::from_secs(40)).await;
    let post_resume_lines: Vec<&str> = post_log
        .lines()
        .filter(|l| l.contains("post_resume_iter"))
        .collect();
    assert_eq!(
        post_resume_lines.len(),
        5,
        "expected 5 post-resume reads; got {}:\n{post_log}",
        post_resume_lines.len(),
    );
    let want = std::str::from_utf8(expected_post_resume).unwrap();
    let other = if expected_post_resume == SENTINEL_A {
        "BBBB"
    } else {
        "AAAA"
    };
    let want_count = post_resume_lines
        .iter()
        .filter(|l| l.contains(want))
        .count();
    let other_count = post_resume_lines
        .iter()
        .filter(|l| l.contains(other))
        .count();
    assert_eq!(
        want_count,
        5,
        "expected all 5 post-resume reads to return {want}; \
         got want={want_count} other={other_count} in:\n{}",
        post_resume_lines.join("\n"),
    );
    assert_eq!(
        other_count, 0,
        "no post-resume read should return {other}; got want={want_count} other={other_count}",
    );

    let _ = fc2.start_kill();
    let _ = fc2.wait().await;
}

/// Write a sentinel at offset 0 of an otherwise-zero file, sized so the
/// VM sees a sensible virtio-blk geometry.
async fn write_padded_file(path: &Path, sentinel: &[u8], total_bytes: u64) {
    let mut f = tokio::fs::File::create(path).await.expect("create bundle");
    f.write_all(sentinel).await.expect("write sentinel");
    f.set_len(total_bytes).await.expect("pad to size");
    f.flush().await.expect("flush");
}

/// Loop-mount the rootfs, write `/sbin/init.experiment`, unmount. The
/// script sleeps through the snapshot window, then reads /dev/vdb (the
/// aux RO drive — second drive after the rootfs) 5× after resume.
async fn install_init_script(rootfs: &Path, work: &Path) {
    let mnt = work.join("mnt");
    tokio::fs::create_dir_all(&mnt).await.expect("mkdir mnt");

    let init_script = "#!/bin/sh\n\
         exec </dev/console >/dev/console 2>/dev/console\n\
         echo \"[VM] init.experiment booted\"\n\
         echo \"[VM] sleeping 12s (snapshot window)\"\n\
         sleep 12\n\
         echo \"[VM] post-resume reads (5x)\"\n\
         for i in 1 2 3 4 5; do\n\
           dd if=/dev/vdb bs=4 count=1 iflag=direct 2>/dev/null || dd if=/dev/vdb bs=4 count=1 2>/dev/null\n\
           echo \" <- post_resume_iter_$i\"\n\
           sleep 1\n\
         done\n\
         while :; do sleep 60; done\n";

    let mount_status = Command::new("sudo")
        .args([
            "mount",
            "-o",
            "loop",
            rootfs.to_str().unwrap(),
            mnt.to_str().unwrap(),
        ])
        .status()
        .await
        .expect("spawn mount");
    assert!(mount_status.success(), "sudo mount failed");

    let mut tee = Command::new("sudo")
        .args(["tee", mnt.join("sbin/init.experiment").to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn tee");
    tee.stdin
        .as_mut()
        .unwrap()
        .write_all(init_script.as_bytes())
        .await
        .expect("write script via tee");
    let tee_status = tee.wait().await.expect("wait tee");
    assert!(tee_status.success(), "sudo tee failed");

    let chmod_status = Command::new("sudo")
        .args([
            "chmod",
            "+x",
            mnt.join("sbin/init.experiment").to_str().unwrap(),
        ])
        .status()
        .await
        .expect("spawn chmod");
    assert!(chmod_status.success(), "sudo chmod failed");

    let umount_status = Command::new("sudo")
        .args(["umount", mnt.to_str().unwrap()])
        .status()
        .await
        .expect("spawn umount");
    assert!(umount_status.success(), "sudo umount failed");
}

/// Poll `log_path` until `n` `"post_resume_iter"` lines appear or
/// `timeout` elapses; returns the final log either way.
async fn wait_for_post_resume_reads(log_path: &Path, n: usize, timeout: Duration) -> String {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let log = std::fs::read_to_string(log_path).unwrap_or_default();
        let hits = log
            .lines()
            .filter(|l| l.contains("post_resume_iter"))
            .count();
        if hits >= n || std::time::Instant::now() >= deadline {
            return log;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn spawn_firecracker(api_sock: &Path, log_path: &Path) -> Child {
    let log = std::fs::File::create(log_path).expect("create fc log");
    let log_clone = log.try_clone().expect("dup log fd");
    Command::new("firecracker")
        .args(["--api-sock", api_sock.to_str().unwrap()])
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_clone))
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn firecracker")
}

/// Boot config: rootfs (RW root device) + the pinned bundle generation
/// as a read-only second drive — matching the production attach in
/// `create_in_jail_after_net` (content-addressed `path_on_host`).
async fn configure_boot(
    client: &FirecrackerClient,
    kernel: &Path,
    rootfs: &Path,
    pinned_generation: &Path,
) {
    client
        .put_machine_config(&MachineConfig {
            vcpu_count: 1,
            mem_size_mib: 128,
            smt: false,
            track_dirty_pages: false,
            cpu_template: None,
        })
        .await
        .expect("put_machine_config");
    client
        .put_boot_source(&BootSource {
            kernel_image_path: kernel.to_string_lossy().into_owned(),
            boot_args: "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/init.experiment".into(),
            initrd_path: None,
        })
        .await
        .expect("put_boot_source");
    client
        .put_drive(&DriveConfig {
            drive_id: "rootfs".into(),
            path_on_host: rootfs.to_string_lossy().into_owned(),
            is_root_device: true,
            is_read_only: false,
        })
        .await
        .expect("put_drive rootfs");
    client
        .put_drive(&DriveConfig {
            drive_id: "skills".into(),
            path_on_host: pinned_generation.to_string_lossy().into_owned(),
            is_root_device: false,
            is_read_only: true,
        })
        .await
        .expect("put_drive aux RO bundle");
}

/// Like `configure_boot` but attaches N aux RO drives (`dyn-0..dyn-{N-1}`) —
/// the ADR 0055 reserved-slot device model. Each `put_drive` that exceeds FC's
/// GSI pool fails loudly with its slot index.
async fn configure_boot_n(
    client: &FirecrackerClient,
    kernel: &Path,
    rootfs: &Path,
    aux: &[std::path::PathBuf],
) {
    client
        .put_machine_config(&MachineConfig {
            vcpu_count: 1,
            mem_size_mib: 128,
            smt: false,
            track_dirty_pages: false,
            cpu_template: None,
        })
        .await
        .expect("put_machine_config");
    client
        .put_boot_source(&BootSource {
            kernel_image_path: kernel.to_string_lossy().into_owned(),
            boot_args: "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/init.experiment".into(),
            initrd_path: None,
        })
        .await
        .expect("put_boot_source");
    client
        .put_drive(&DriveConfig {
            drive_id: "rootfs".into(),
            path_on_host: rootfs.to_string_lossy().into_owned(),
            is_root_device: true,
            is_read_only: false,
        })
        .await
        .expect("put_drive rootfs");
    for (i, path) in aux.iter().enumerate() {
        client
            .put_drive(&DriveConfig {
                drive_id: engram_core::types::sandbox::AuxRoDrive::slot_drive_id(i),
                path_on_host: path.to_string_lossy().into_owned(),
                is_root_device: false,
                is_read_only: true,
            })
            .await
            .unwrap_or_else(|e| panic!("put_drive dyn_{i} failed (FC device ceiling?): {e}"));
    }
}
