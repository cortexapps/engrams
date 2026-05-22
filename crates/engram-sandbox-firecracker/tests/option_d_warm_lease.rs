//! ADR 0014 option-D end-to-end integration test.
//!
//! Exercises the full warm-lease shape against real Firecracker:
//!
//! 1. Boot a microVM with rootfs + STUB harness ext4 attached as
//!    `/dev/vdb` (16 MiB empty ext4).
//! 2. Snapshot it via the FC backend's `snapshot()` — the snapshot
//!    captures the bake-time state with the stub still attached.
//! 3. Destroy the source sandbox.
//! 4. Build a *session-specific* harness ext4 with different
//!    contents so we can prove the kernel reads from the new file
//!    after the swap.
//! 5. Restore from the snapshot.
//! 6. Call `backend.swap_harness_drive(id, session_harness_path)`
//!    — the option-D primitive: pause → patch_drive → resume.
//! 7. Mount the device inside the (now-restored) VM and read
//!    bytes from the session harness, confirm they match what we
//!    just installed.
//!
//! This is the end-to-end version of `patch_drive_swap.rs`'s
//! lower-level test. Where patch_drive_swap drives the FC API
//! directly, this one goes through `FirecrackerBackend` so the
//! full restore_in_jail / start_agent flow gets covered.
//!
//! Run via:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-sandbox-firecracker --test option_d_warm_lease \
//!     -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const STUB_SENTINEL: &[u8] = b"BAKE";
const SESSION_SENTINEL: &[u8] = b"SESS";
const HARNESS_SIZE: u64 = 16 * 1024 * 1024;

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + sudo mount; run with --ignored on the dev VM"]
async fn warm_lease_swaps_harness_and_guest_reads_session_bytes() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !common::require_bin("mount") || !common::require_bin("umount") {
        return;
    }

    let work = tempfile::tempdir().expect("tempdir");
    let work = work.path();

    // ── prepare the two harness images ──
    let stub_harness = work.join("stub-harness.ext4");
    let session_harness = work.join("session-harness.ext4");
    write_padded_file(&stub_harness, STUB_SENTINEL, HARNESS_SIZE).await;
    write_padded_file(&session_harness, SESSION_SENTINEL, HARNESS_SIZE).await;

    // ── prepare a writable copy of the test rootfs with our init
    //   script. Same shape as patch_drive_swap.rs; reuse here so
    //   we don't have to bake a custom rootfs. ──
    let rootfs = work.join("rootfs.ext4");
    tokio::fs::copy(&env.rootfs, &rootfs)
        .await
        .expect("copy rootfs");
    install_init_script(&rootfs, work).await;

    // ── stand up the FC backend ──
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
    cfg.net_pool = None;
    cfg.default_boot_args =
        "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/init.experiment".into();
    let backend = FirecrackerBackend::new(work, cfg);

    let spec = SandboxSpec {
        image: "option-d-test".into(),
        rootfs_source: Some(rootfs.clone()),
        image_uri: None,
        harness_pack_uri: None,
        harness_substrate: Some(stub_harness.clone()),
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 128 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: Default::default(),
        workdir: None,
        network: Default::default(),
    };

    // Bake-time: source sandbox boots with stub attached as /dev/vdb.
    let source_id = backend.create(spec.clone()).await.expect("create source");
    // Let the kernel boot + init.experiment enter its initial state.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Snapshot at bake-time (with stub attached, kernel hasn't
    // touched /dev/vdb yet thanks to init.experiment's startup
    // ordering — see the script).
    let metadata = backend.snapshot(source_id).await.expect("snapshot");
    backend.destroy(source_id).await.expect("destroy source");

    // ── option-D restore + swap ──
    let restore_start = Instant::now();
    let mut metadata_with_session = metadata.clone();
    // Re-pointing the spec's harness_substrate so the restored
    // sandbox's LiveSandbox tracks the session-specific path.
    // FirecrackerBackend::restore_in_jail re-installs the canonical
    // harness symlink at `<work>/harness/<source_sandbox_id>.ext4`
    // based on `manifest.spec.harness_substrate`; we want it
    // pointing at our session harness so the symlink target matches
    // what swap_harness_drive will later set as path_on_host.
    metadata_with_session.id = metadata.id;
    let restored_id = backend
        .restore(metadata_with_session.clone())
        .await
        .expect("restore");
    let restore_elapsed = restore_start.elapsed();

    // The option-D operation: pause → patch_drive → resume.
    let swap_start = Instant::now();
    backend
        .swap_harness_drive(restored_id, session_harness.clone())
        .await
        .expect("swap_harness_drive");
    let swap_elapsed = swap_start.elapsed();

    // Wait for the in-VM init script's dd loop to fire post-swap.
    // The script reads /dev/vdb every 1s starting at boot, so
    // within ~3 s we should see SESS bytes in the serial log.
    tokio::time::sleep(Duration::from_secs(4)).await;

    // The serial log was captured by FC into the sandbox's jail
    // dir at `firecracker.log`.
    let log_path = work.join(restored_id.to_string()).join("firecracker.log");
    let log_bytes = sudo_read(&log_path).await;
    let log = String::from_utf8_lossy(&log_bytes);

    // Must NOT see the stub sentinel (BAKE) — if we do, the swap
    // didn't take effect.
    assert!(
        !log.contains("BAKE"),
        "post-swap serial log should not contain stub sentinel; got:\n{log}",
    );
    // Must see the session sentinel — if we don't, the swap
    // happened but the kernel didn't observe the new bytes.
    assert!(
        log.contains("SESS"),
        "post-swap serial log MUST contain session sentinel; got:\n{log}",
    );

    eprintln!("=== option-D end-to-end timing ===");
    eprintln!("restore elapsed: {:?}", restore_elapsed);
    eprintln!("swap elapsed:    {:?}", swap_elapsed);
    eprintln!("==================================");

    // Swap should be sub-100ms on Firecracker; loose bound at
    // 500ms so CI noise doesn't flake.
    assert!(
        swap_elapsed < Duration::from_millis(500),
        "swap_harness_drive should be sub-500ms; got {swap_elapsed:?}",
    );

    // Cleanup.
    let _ = backend.destroy(restored_id).await;
}

async fn write_padded_file(path: &Path, sentinel: &[u8], total_bytes: u64) {
    let mut f = tokio::fs::File::create(path).await.expect("create harness");
    f.write_all(sentinel).await.expect("write sentinel");
    f.set_len(total_bytes).await.expect("pad");
    f.flush().await.expect("flush");
}

/// init.experiment continuously reads /dev/vdb and prints the
/// first 4 bytes. Pre-snapshot: kernel only emits BAKE if dd has
/// already run. To exercise the option-D "snapshot taken before
/// guest touches vdb" pattern, we delay the first read by a small
/// sleep — but still need to read at SOME point so we can verify
/// the swap. So: sleep 2s, then dd in a loop. Snapshot happens at
/// boot+3s (in the test driver), so kernel will have read once,
/// see BAKE, then we snapshot, then we swap. After resume, the
/// loop continues — next read SHOULD return SESS.
///
/// This intentionally exercises the "page cache staleness" path
/// (we read before snapshot, polluting cache); the FC queue-kick
/// + virtio-blk size-change handling has to invalidate.
async fn install_init_script(rootfs: &Path, work: &Path) {
    let mnt = work.join("mnt");
    tokio::fs::create_dir_all(&mnt).await.expect("mkdir mnt");

    let init_script = "#!/bin/sh
exec </dev/console >/dev/console 2>/dev/console
echo '[VM] init.experiment booted (option-D test)'
sleep 2
for i in 1 2 3 4 5 6 7 8 9 10; do
  dd if=/dev/vdb bs=4 count=1 2>/dev/null
  echo \" <- iter_$i\"
  sleep 1
done
while :; do sleep 60; done
";

    let status = Command::new("sudo")
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
    assert!(status.success(), "sudo mount failed");

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
        .expect("write script");
    assert!(tee.wait().await.unwrap().success(), "tee failed");

    let chmod = Command::new("sudo")
        .args([
            "chmod",
            "+x",
            mnt.join("sbin/init.experiment").to_str().unwrap(),
        ])
        .status()
        .await
        .expect("spawn chmod");
    assert!(chmod.success(), "chmod failed");

    let umount = Command::new("sudo")
        .args(["umount", mnt.to_str().unwrap()])
        .status()
        .await
        .expect("spawn umount");
    assert!(umount.success(), "umount failed");
}

async fn sudo_read(path: &Path) -> Vec<u8> {
    let out = Command::new("sudo")
        .args(["cat", path.to_str().unwrap()])
        .output()
        .await
        .expect("sudo cat");
    out.stdout
}
