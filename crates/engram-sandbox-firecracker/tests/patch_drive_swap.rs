//! ADR 0014 option-D mechanism test.
//!
//! Confirms that `FirecrackerClient::patch_drive` on a paused-from-
//! snapshot microVM swaps the host file backing a virtio-blk device,
//! and that the guest sees the new file's bytes on its next read of
//! that device.
//!
//! This is the primitive the warm-pool restore path uses to per-
//! session-bind the harness substrate: snapshot is taken at
//! bootstrap-on-accept() (before bootstrap mounts the harness
//! device), restore loads paused, host PATCHes the harness drive to
//! the session's harness ext4, host resumes, bootstrap mounts vdb +
//! exec's the harness — all from the same snapshot regardless of
//! which harness the session asked for.
//!
//! Two variants exercise the mechanism:
//!
//!   - `swap_after_paused_load_works`: snapshot is taken before the
//!     guest reads vdb at all (production option-D shape). Post-
//!     resume guest reads return the new file's bytes.
//!   - `swap_after_paused_load_invalidates_stale_page_cache`: guest
//!     reads vdb 3× pre-snapshot (page cache polluted with old
//!     bytes). Post-resume reads still return the new bytes — FC's
//!     "Artificially kick devices" + virtio-blk size-change uevent
//!     invalidates the cache. Belt-and-suspenders; not on the prod
//!     option-D critical path, but useful coverage.
//!
//! Both tests need to write `/sbin/init.experiment` into the rootfs.
//! That requires `sudo mount`/`umount` because there's no portable
//! way to inject a file into an ext4 image without mounting. Same
//! gating as the other ignored FC integration tests (Linux + KVM +
//! firecracker on PATH + fetched test artifacts), plus mount(8) on
//! PATH.
//!
//! Run via:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-sandbox-firecracker --test patch_drive_swap \
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

/// Two short ASCII sentinels — different bytes at offset 0 so the
/// guest's `dd if=/dev/vdb bs=4 count=1` produces a printable line
/// the test can grep for on the serial console.
const SENTINEL_A: &[u8] = b"AAAA";
const SENTINEL_B: &[u8] = b"BBBB";

/// 16 MiB — big enough for a clean virtio-blk geometry log
/// (`new size: 32768 512-byte logical blocks`) and to dodge the
/// "capacity change from 0 to <small>" quirk that 4 KiB images
/// produced in the spike.
const HARNESS_SIZE: u64 = 16 * 1024 * 1024;

/// Production-shape: snapshot is taken *before* the guest touches
/// /dev/vdb. PATCH after load-paused swaps the backing file, resume
/// proceeds, guest reads return the new file's bytes.
#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + sudo mount; run with --ignored on the dev VM"]
async fn swap_after_paused_load_works() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !common::require_bin("mount") || !common::require_bin("umount") {
        return;
    }

    run_swap_scenario(&env, /*pre_snapshot_reads=*/ false).await;
}

/// Belt-and-suspenders: 3× pre-snapshot reads pollute the kernel
/// page cache with the old file's bytes. Post-PATCH reads should
/// still return the new bytes — FC's queue-kick + virtio-blk size-
/// change uevent invalidates the cache automatically.
#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + sudo mount; run with --ignored on the dev VM"]
async fn swap_after_paused_load_invalidates_stale_page_cache() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !common::require_bin("mount") || !common::require_bin("umount") {
        return;
    }

    run_swap_scenario(&env, /*pre_snapshot_reads=*/ true).await;
}

async fn run_swap_scenario(env: &common::FcEnv, pre_snapshot_reads: bool) {
    let work = tempfile::tempdir().expect("tempdir");
    let work = work.path();

    // ── prepare harness images ──
    let harness_a = work.join("harness-A.img");
    let harness_b = work.join("harness-B.img");
    write_padded_file(&harness_a, SENTINEL_A, HARNESS_SIZE).await;
    write_padded_file(&harness_b, SENTINEL_B, HARNESS_SIZE).await;

    // ── prepare rootfs copy with init.experiment ──
    let rootfs = work.join("rootfs.ext4");
    tokio::fs::copy(&env.rootfs, &rootfs)
        .await
        .expect("copy rootfs");
    install_init_script(&rootfs, work, pre_snapshot_reads).await;

    // ── FC #1: boot with harness-A, snapshot ──
    let fc1_log = work.join("fc1.log");
    let fc1_api = work.join("fc1.sock");
    let mut fc1 = spawn_firecracker(&fc1_api, &fc1_log).await;
    // FC sometimes takes a beat to bind its API socket; the
    // FirecrackerClient handles connect-retry, but a tiny sleep
    // keeps the first PUT from waiting.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let client1 = FirecrackerClient::new(&fc1_api);
    configure_boot(&client1, &env.kernel, &rootfs, &harness_a).await;
    client1
        .put_action(ActionType::InstanceStart)
        .await
        .expect("instance start");

    // Give the guest enough time to:
    //  - boot
    //  - run init.experiment
    //  - in the staleness variant, do its 3× pre-snapshot reads
    //  - enter the 12s sleep
    // 6s is comfortably inside the sleep window for both variants.
    tokio::time::sleep(Duration::from_secs(6)).await;

    let pre_log = std::fs::read_to_string(&fc1_log).unwrap_or_default();
    if pre_snapshot_reads {
        assert!(
            pre_log.matches("AAAA").count() >= 3,
            "pre-snapshot variant must observe at least 3 AAAA hits before snapshot; got:\n{pre_log}",
        );
    } else {
        assert!(
            !pre_log.contains("AAAA"),
            "production-shape variant must NOT touch vdb pre-snapshot; got:\n{pre_log}",
        );
    }
    assert!(
        !pre_log.contains("BBBB"),
        "harness-B should never appear before the patch; got:\n{pre_log}",
    );

    let snapshot_paths = client1
        .create_snapshot(work)
        .await
        .expect("create snapshot");
    // create_snapshot resumes the VM internally; pause + kill FC #1
    // to free the API socket for FC #2.
    let _ = fc1.start_kill();
    let _ = fc1.wait().await;

    // ── FC #2: load paused, PATCH drive, resume ──
    let fc2_log = work.join("fc2.log");
    let fc2_api = work.join("fc2.sock");
    let mut fc2 = spawn_firecracker(&fc2_api, &fc2_log).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let client2 = FirecrackerClient::new(&fc2_api);

    client2
        .load_snapshot_paused(&snapshot_paths)
        .await
        .expect("load_snapshot_paused");

    // *** THE MECHANISM UNDER TEST ***
    client2
        .patch_drive("harness", &harness_b)
        .await
        .expect("patch_drive");

    client2
        .patch_vm_state(VmState::Resumed)
        .await
        .expect("resume after patch");

    // Poll the serial log for all 5 post-resume reads. Snapshot's
    // monotonic clock continues, so the in-VM 12s sleep ends some
    // wall-clock seconds after resume — exact timing varies with
    // the variant (the staleness variant ran 3 pre-snapshot reads
    // + sleeps before the 12s sleep, so its post-resume reads come
    // later). 40s is a generous ceiling.
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
    let bbbb_count = post_resume_lines
        .iter()
        .filter(|l| l.contains("BBBB"))
        .count();
    let aaaa_count = post_resume_lines
        .iter()
        .filter(|l| l.contains("AAAA"))
        .count();
    assert_eq!(
        bbbb_count, 5,
        "expected all 5 post-resume reads to return BBBB (harness-B); got AAAA={aaaa_count} BBBB={bbbb_count} in:\n{}",
        post_resume_lines.join("\n"),
    );
    assert_eq!(
        aaaa_count, 0,
        "no post-resume read should return AAAA (cache must be invalidated); got AAAA={aaaa_count} BBBB={bbbb_count}",
    );

    let _ = fc2.start_kill();
    let _ = fc2.wait().await;
}

/// Write a sentinel at offset 0 of an otherwise-zero file. Sized to
/// `total_bytes` so the VM sees a sensible virtio-blk geometry.
async fn write_padded_file(path: &Path, sentinel: &[u8], total_bytes: u64) {
    let mut f = tokio::fs::File::create(path).await.expect("create harness");
    f.write_all(sentinel).await.expect("write sentinel");
    f.set_len(total_bytes).await.expect("pad to size");
    f.flush().await.expect("flush");
}

/// Loop-mount the rootfs, write `/sbin/init.experiment`, unmount.
/// `with_pre_reads` controls whether the script reads /dev/vdb
/// 3× before the snapshot window — used by the staleness variant.
async fn install_init_script(rootfs: &Path, work: &Path, with_pre_reads: bool) {
    let mnt = work.join("mnt");
    tokio::fs::create_dir_all(&mnt).await.expect("mkdir mnt");

    let pre_reads_block = if with_pre_reads {
        "echo \"[VM] pre-snapshot reads (3x)\"\n\
         for i in 1 2 3; do\n\
           dd if=/dev/vdb bs=4 count=1 2>/dev/null\n\
           echo \" <- pre_snap_iter_$i\"\n\
           sleep 1\n\
         done\n"
    } else {
        ""
    };
    let init_script = format!(
        "#!/bin/sh\n\
         exec </dev/console >/dev/console 2>/dev/console\n\
         echo \"[VM] init.experiment booted\"\n\
         {pre_reads_block}\
         echo \"[VM] sleeping 12s (snapshot window)\"\n\
         sleep 12\n\
         echo \"[VM] post-resume reads (5x)\"\n\
         for i in 1 2 3 4 5; do\n\
           dd if=/dev/vdb bs=4 count=1 2>/dev/null\n\
           echo \" <- post_resume_iter_$i\"\n\
           sleep 1\n\
         done\n\
         while :; do sleep 60; done\n"
    );

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

    // Write the script via `sudo tee` (the rootfs is owned by
    // root inside the mount). Pipe init_script through stdin.
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

/// Poll `log_path` until `n` lines containing `"post_resume_iter"`
/// appear, or `timeout` elapses. Returns the final log contents
/// either way so the caller can produce a useful assertion message.
async fn wait_for_post_resume_reads(log_path: &Path, n: usize, timeout: Duration) -> String {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let log = std::fs::read_to_string(log_path).unwrap_or_default();
        let hits = log
            .lines()
            .filter(|l| l.contains("post_resume_iter"))
            .count();
        if hits >= n {
            return log;
        }
        if std::time::Instant::now() >= deadline {
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

async fn configure_boot(
    client: &FirecrackerClient,
    kernel: &Path,
    rootfs: &Path,
    harness_a: &Path,
) {
    client
        .put_machine_config(&MachineConfig {
            vcpu_count: 1,
            mem_size_mib: 128,
            smt: false,
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
            drive_id: "harness".into(),
            path_on_host: harness_a.to_string_lossy().into_owned(),
            is_root_device: false,
            is_read_only: false,
        })
        .await
        .expect("put_drive harness");
}
