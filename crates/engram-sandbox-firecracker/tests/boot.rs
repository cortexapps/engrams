//! End-to-end smoke test: bring up a Firecracker microVM with our HTTP
//! client and verify the kernel banner shows up on serial console.
//!
//! Gated `#[ignore]` because it needs:
//!   - Linux + KVM (`/dev/kvm`)
//!   - `firecracker` binary on $PATH
//!   - Test artifacts (kernel + ext4 rootfs) at `$FC_TEST_KERNEL` /
//!     `$FC_TEST_ROOTFS`
//!
//! Bootstrapping the artifacts is a one-shot
//! `scripts/fetch-fc-test-artifacts.sh` on the dev VM. Run the test:
//!
//! ```sh
//! cargo test -p engram-sandbox-firecracker --test boot -- --ignored --nocapture
//! ```
//!
//! This proves the entire `FirecrackerClient` PUT path works end-to-end
//! against a real Firecracker process; subsequent slices wire it into
//! `FirecrackerBackend::create` and add snapshot/restore.

#![cfg(target_os = "linux")]

mod common;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use engram_sandbox_firecracker::{
    ActionType, BootSource, DriveConfig, FirecrackerClient, MachineConfig,
};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker; run with --ignored on the dev VM"]
async fn boot_microvm_and_capture_kernel_banner() {
    let env = match common::fc_preflight() {
        Some(e) => e,
        None => return,
    };
    let kernel = env.kernel;
    let rootfs = env.rootfs;
    for (label, p) in [("kernel", &kernel), ("rootfs", &rootfs)] {
        assert!(p.exists(), "{label} missing at {}", p.display());
    }

    // ---- spawn firecracker -----------------------------------------
    // Tempdir holds the API socket; cleanup is automatic on drop.
    let socket_dir = tempfile::tempdir().expect("tempdir");
    let socket = socket_dir.path().join("fc.sock");

    let mut fc = Command::new("firecracker")
        .args(["--api-sock", socket.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true) // belt + suspenders so a panic still tears down the VM
        .spawn()
        .expect("spawn firecracker");
    let mut stdout = fc.stdout.take().expect("piped stdout");
    let mut stderr = fc.stderr.take().expect("piped stderr");

    wait_for_socket(&socket, Duration::from_secs(5))
        .await
        .expect("API socket did not appear; firecracker likely crashed");

    // ---- configure + start -----------------------------------------
    let client = FirecrackerClient::new(&socket);
    client
        .put_machine_config(&MachineConfig {
            vcpu_count: 1,
            mem_size_mib: 128,
            smt: false,
            track_dirty_pages: false,
            cpu_template: None,
        })
        .await
        .expect("PUT /machine-config");
    client
        .put_boot_source(&BootSource {
            kernel_image_path: kernel.to_string_lossy().into_owned(),
            // pci=off / reboot=k / panic=1 are the standard Firecracker
            // boot args: no PCI bus, triple-fault reboot, fast panic.
            boot_args: "console=ttyS0 reboot=k panic=1 pci=off".into(),
            initrd_path: None,
        })
        .await
        .expect("PUT /boot-source");
    client
        .put_drive(&DriveConfig {
            drive_id: "rootfs".into(),
            path_on_host: rootfs.to_string_lossy().into_owned(),
            is_root_device: true,
            // RO so we never touch the cached test image; also avoids
            // journal recovery if the FS was last unmounted uncleanly.
            is_read_only: true,
        })
        .await
        .expect("PUT /drives/rootfs");
    client
        .put_action(ActionType::InstanceStart)
        .await
        .expect("PUT /actions InstanceStart");

    // ---- read serial console for up to 5s --------------------------
    let mut serial = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut buf = vec![0u8; 16 * 1024];
    while tokio::time::Instant::now() < deadline {
        let read_fut = stdout.read(&mut buf);
        match tokio::time::timeout(Duration::from_millis(250), read_fut).await {
            Ok(Ok(0)) => break, // EOF — firecracker's stdout closed
            Ok(Ok(n)) => serial.push_str(&String::from_utf8_lossy(&buf[..n])),
            Ok(Err(_)) | Err(_) => {} // io error or 250ms idle: keep polling
        }
        if serial.contains("Linux version") {
            break;
        }
    }

    // Slurp any stderr too, just so a failure has full diagnostics.
    let mut stderr_buf = String::new();
    let _ = tokio::time::timeout(
        Duration::from_millis(100),
        stderr.read_to_string(&mut stderr_buf),
    )
    .await;

    // ---- cleanup BEFORE assertion (so failures don't leak the VM) --
    let _ = fc.kill().await;
    let _ = fc.wait().await;
    drop(socket_dir);

    // ---- assert the kernel actually booted -------------------------
    assert!(
        serial.contains("Linux version"),
        "kernel banner not seen on serial.\n--- stdout (first 2KB) ---\n{}\n--- stderr ---\n{}",
        serial.chars().take(2000).collect::<String>(),
        stderr_buf.chars().take(2000).collect::<String>(),
    );
}

async fn wait_for_socket(path: &Path, budget: Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if path.exists() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "socket {} did not appear within {:?}",
                path.display(),
                budget
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
