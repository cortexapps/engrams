//! ADR 0027 aux read-only drive mechanism test.
//!
//! The RO-mount skills/MCP engine attaches a fleet-wide, content-
//! addressed bundle (skills / playwright squashfs) as an *additional*
//! read-only virtio-blk drive. Unlike the per-session harness drive
//! (ADR 0014 option-D, exercised by `patch_drive_swap`), the bundle
//! path is the SAME on every host — a stable host-wide symlink baked
//! into the FC-host image. So the snapshot embeds that path and restore
//! re-anchors by mere presence: NO `patch_drive`, just "the file is
//! there." This test pins that property and the design choice that
//! makes a version roll safe.
//!
//! Two variants:
//!
//!   - `aux_ro_drive_reopens_at_embedded_path`: attach an RO drive at a
//!     plain host path, boot, snapshot, load-paused on a SECOND FC,
//!     resume WITHOUT patching. The guest's post-resume read of the
//!     drive returns the original bytes — FC reopened the embedded path
//!     on its own.
//!   - `aux_ro_drive_symlink_roll_serves_new_bytes`: the embedded path
//!     is a symlink. Repoint it at a different backing file BETWEEN
//!     snapshot and restore (a bundle version roll), resume without
//!     patching, and the guest sees the NEW bytes — FC resolves the
//!     symlink at load_snapshot open time. This is why we embed the
//!     stable symlink rather than a digest-pinned filename.
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

/// Production-shape: the bundle path is present and identical on the
/// receiver, so restore re-anchors with no PATCH.
#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + sudo mount; run with --ignored on the dev VM"]
async fn aux_ro_drive_reopens_at_embedded_path() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !common::require_bin("mount") || !common::require_bin("umount") {
        return;
    }
    // No symlink roll: the same file backs the drive across both FCs.
    run_scenario(&env, /*roll=*/ false, SENTINEL_A).await;
}

/// Version-roll shape: the embedded path is a symlink; we repoint it at
/// a new backing file between snapshot and restore. The guest must see
/// the new bytes — proving the stable-symlink indirection lets a bundle
/// roll without invalidating existing snapshots.
#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + sudo mount; run with --ignored on the dev VM"]
async fn aux_ro_drive_symlink_roll_serves_new_bytes() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !common::require_bin("mount") || !common::require_bin("umount") {
        return;
    }
    // Symlink roll: boot reads A, restore reads B after the repoint.
    run_scenario(&env, /*roll=*/ true, SENTINEL_B).await;
}

async fn run_scenario(env: &common::FcEnv, roll: bool, expected_post_resume: &[u8]) {
    let work = tempfile::tempdir().expect("tempdir");
    let work = work.path();

    // ── prepare the RO bundle backing file(s) + the path FC opens ──
    //
    // Non-roll: `bundle_path` is a plain file with SENTINEL_A.
    // Roll: `bundle_path` is a symlink → bundle-A.img at boot; we
    // repoint it → bundle-B.img before restore.
    let bundle_path = work.join("bundle.squashfs");
    let bundle_a = work.join("bundle-A.img");
    write_padded_file(&bundle_a, SENTINEL_A, BUNDLE_SIZE).await;
    let bundle_b = work.join("bundle-B.img");
    if roll {
        write_padded_file(&bundle_b, SENTINEL_B, BUNDLE_SIZE).await;
        tokio::fs::symlink(&bundle_a, &bundle_path)
            .await
            .expect("symlink bundle -> A");
    } else {
        tokio::fs::copy(&bundle_a, &bundle_path)
            .await
            .expect("copy bundle A -> path");
    }

    // ── prepare rootfs copy with init.experiment ──
    let rootfs = work.join("rootfs.ext4");
    tokio::fs::copy(&env.rootfs, &rootfs)
        .await
        .expect("copy rootfs");
    install_init_script(&rootfs, work).await;

    // ── FC #1: boot with the RO bundle attached, snapshot ──
    let fc1_log = work.join("fc1.log");
    let fc1_api = work.join("fc1.sock");
    let mut fc1 = spawn_firecracker(&fc1_api, &fc1_log).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client1 = FirecrackerClient::new(&fc1_api);
    configure_boot(&client1, &env.kernel, &rootfs, &bundle_path).await;
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

    // ── version roll: repoint the embedded symlink at the new file ──
    if roll {
        tokio::fs::remove_file(&bundle_path)
            .await
            .expect("rm old symlink");
        tokio::fs::symlink(&bundle_b, &bundle_path)
            .await
            .expect("symlink bundle -> B");
    }

    // ── FC #2: load paused, resume WITHOUT patching ──
    let fc2_log = work.join("fc2.log");
    let fc2_api = work.join("fc2.sock");
    let mut fc2 = spawn_firecracker(&fc2_api, &fc2_log).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let client2 = FirecrackerClient::new(&fc2_api);

    client2
        .load_snapshot_paused(&snapshot_paths)
        .await
        .expect("load_snapshot_paused");

    // *** No patch_drive — re-anchor by presence is the whole point. ***

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
    let want_count = post_resume_lines.iter().filter(|l| l.contains(want)).count();
    let other_count = post_resume_lines
        .iter()
        .filter(|l| l.contains(other))
        .count();
    assert_eq!(
        want_count, 5,
        "expected all 5 post-resume reads to return {want} (roll={roll}); \
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
           dd if=/dev/vdb bs=4 count=1 2>/dev/null\n\
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

/// Boot config: rootfs (RW root device) + the aux bundle as a
/// read-only second drive. The aux drive carries `is_read_only: true`,
/// matching the production attach in `create_in_jail_after_net`.
async fn configure_boot(
    client: &FirecrackerClient,
    kernel: &Path,
    rootfs: &Path,
    bundle_path: &Path,
) {
    client
        .put_machine_config(&MachineConfig {
            vcpu_count: 1,
            mem_size_mib: 128,
            smt: false,
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
            path_on_host: bundle_path.to_string_lossy().into_owned(),
            is_root_device: false,
            is_read_only: true,
        })
        .await
        .expect("put_drive aux RO bundle");
}
