//! ADR 0080: dynamic agentd end-to-end — boot from the bundle, roll the
//! bundle, restore, re-exec.
//!
//! Proves the three properties the design rests on, with one boot + one
//! snapshot + one fresh restore (sized to the property, not to realism):
//!
//! 1. **Boot-from-bundle**: a rootfs carrying only the stage-1 init shim
//!    (no baked agentd) boots by resolving the SYMBOLIC agentd slot
//!    against the host stamp, mounting the bundle, copying agentd to
//!    tmpfs, and exec'ing the copy — `/run/engram/agentd.sha256` matches
//!    the staged generation's stamp.
//! 2. **Re-exec on roll**: a fresh-create restore whose `selected_mounts`
//!    pins a DIFFERENT agentd generation `patch_drive`s the slot in the
//!    paused window; `refresh_agent` reports `Restarted` and the guest
//!    serves with the new generation's stamp — zero recapture.
//! 3. **Idempotence**: a second `refresh_agent` round against the same
//!    guest is `UpToDate` (the guest-side stamp compare; the host-side
//!    `agentd_slot_swapped` fast path is exercised by every other FC
//!    restore test, whose unswapped restores must not RPC at all).
//!
//! Run via:
//!
//! ```sh
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! cargo test -p engram-sandbox-firecracker --test agentd_bundle_reexec -- --ignored --nocapture
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use engram_core::traits::sandbox::{AgentRefresh, SandboxBackend};
use engram_core::types::sandbox::{
    AuxRoDrive, CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec,
};
use engram_rootfs_materializer::{InitInjection, Transport};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};

use common::{drain, fc_preflight, require_bin};

/// Exec `cmd` (sh -c) in the guest, polling until agentd answers —
/// covers both the cold-boot window and the post-re-exec listener
/// re-bind.
async fn exec_sh(backend: &FirecrackerBackend, id: engram_core::SandboxId, cmd: &str) -> String {
    let req = ExecRequest {
        command: vec!["/bin/sh".into(), "-c".into(), cmd.into()],
        stdin: None,
        env: HashMap::new(),
        workdir: None,
        timeout: Some(Duration::from_secs(10)),
        exec_id: None,
        stdout_offset: None,
        stderr_offset: None,
        wake: None,
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let stream = loop {
        match backend.exec_stream(id, req.clone()).await {
            Ok(s) => break s,
            Err(e) if tokio::time::Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(e) => panic!("agentd never answered exec: {e:?}"),
        }
    };
    let (stdout, stderr, exit) = drain(stream.events).await;
    let stdout = String::from_utf8_lossy(&stdout).into_owned();
    assert_eq!(
        exit,
        Some(0),
        "guest cmd `{cmd}` failed: stdout={stdout} stderr={}",
        String::from_utf8_lossy(&stderr),
    );
    stdout.trim().to_string()
}

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; bakes a rootfs and boots a microVM"]
async fn agentd_rolls_via_bundle_without_recapture() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_bin("mksquashfs") {
        return;
    }
    let Some(busybox) = common::find_busybox() else {
        eprintln!("SKIP: no static busybox (apt install busybox-static or set BUSYBOX_STATIC)");
        return;
    };
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let agent =
        Path::new(&manifest).join("../../target/x86_64-unknown-linux-musl/release/engram-agentd");
    if !agent.exists() {
        eprintln!(
            "SKIP: musl engram-agentd not built at {} — \
             cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release",
            agent.display(),
        );
        return;
    }

    // ---- 1. Bake a shim-only rootfs (NO agentd inside) ----
    let images = tempfile::tempdir().expect("images dir");
    let chunk_root = tempfile::tempdir().expect("chunk store root");
    let blob: std::sync::Arc<dyn engram_core::traits::BlobStorage> = std::sync::Arc::new(
        engram_storage_local::LocalBlobStorage::new(chunk_root.path().to_path_buf()),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);
    let outcome = common::bake_fixture_ext4(
        &images.path().join("rootfs.ext4"),
        &chunk_store,
        &busybox,
        Some(InitInjection {
            vsock_port: ENGRAM_AGENTD_PORT,
            transport: Transport::Vsock,
            init_script: None,
        }),
        |_tree| Ok(()),
    )
    .await;

    // ---- 2. Stage generation v1 and boot with the SYMBOLIC slot ----
    // Rootfs outlives the original sandbox's jail (restore reopens it).
    let work = tempfile::tempdir().expect("work dir");
    let bundle_dir = work.path().join("bundles");
    let v1 = common::stage_agentd_bundle(&bundle_dir, &agent);

    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = None; // unprivileged test — see lifecycle.rs
    cfg.bundle_dir = bundle_dir.clone();
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    let spec = SandboxSpec {
        image: "agentd-reexec-test".into(),
        rootfs_source: Some(outcome.rootfs_path),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: 256 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        // Symbolic — the backend must resolve it to the staged `agentd`
        // stamp key (the capture-path resolution this test pins).
        aux_ro_drives: vec![AuxRoDrive::reserved_slot(AuxRoDrive::AGENTD_SLOT_INDEX)],
        swap_mib: None,
    };
    let original = backend.create(spec).await.expect("create");

    // Property 1: the guest runs the v1 generation, staged from the bundle.
    let booted = exec_sh(&backend, original, "cat /run/engram/agentd.sha256").await;
    assert_eq!(
        booted, v1.binary_sha,
        "stage-1 init must copy+exec the staged bundle's agentd",
    );

    // ---- 3. Snapshot (the capture stand-in), destroy the original ----
    let metadata = backend.snapshot(original).await.expect("snapshot");
    backend.destroy(original).await.expect("destroy original");

    // ---- 4. Roll agentd: stage v2 (different bytes → different stamps) ----
    // Trailing padding is ignored by the ELF loader, so v2 is byte-different
    // yet still executable — the cheapest honest "new agentd build".
    let v2_bin = work.path().join("agentd-v2");
    let mut bytes = std::fs::read(&agent).expect("read agentd");
    bytes.push(0);
    std::fs::write(&v2_bin, bytes).expect("write v2 binary");
    let v2 = common::stage_agentd_bundle(&bundle_dir, &v2_bin);
    assert_ne!(v1.binary_sha, v2.binary_sha);
    // v1's squashfs stays staged content-addressed (the snapshot pins it);
    // stage_agentd_bundle only repointed current.json.
    assert!(bundle_dir
        .join(AuxRoDrive::staged_file_name(&v1.squashfs_sha))
        .exists());

    // ---- 5. Fresh restore pinning v2, then RefreshAgent ----
    let restored = backend
        .restore_fresh(
            metadata,
            vec![AuxRoDrive {
                drive_id: AuxRoDrive::slot_drive_id(AuxRoDrive::AGENTD_SLOT_INDEX),
                guest_mount: AuxRoDrive::slot_guest_mount(AuxRoDrive::AGENTD_SLOT_INDEX),
                fs_type: "squashfs".into(),
                sha256: Some(v2.squashfs_sha.clone()),
            }],
        )
        .await
        .expect("fresh restore with v2 agentd selected");

    // Property 2: the captured (v1) agentd adopts v2 and re-execs.
    let refreshed = backend
        .refresh_agent(restored)
        .await
        .expect("refresh_agent");
    assert_eq!(
        refreshed,
        AgentRefresh::Restarted,
        "a swapped agentd slot must re-exec the guest agentd",
    );
    let serving = exec_sh(&backend, restored, "cat /run/engram/agentd.sha256").await;
    assert_eq!(
        serving, v2.binary_sha,
        "post-re-exec the guest must run (and stamp) the v2 generation",
    );

    // Property 3: a second round is UpToDate — the guest-side stamp
    // compare (this sandbox's host-side `agentd_slot_swapped` stays true,
    // so the RPC runs and the GUEST reports nothing to adopt). The
    // host-side fast path (`agentd_slot_swapped == false` ⇒ no RPC at
    // all) is what every unswapped restore in the rest of the suite rides.
    let again = backend
        .refresh_agent(restored)
        .await
        .expect("second refresh_agent");
    assert_eq!(
        again,
        AgentRefresh::UpToDate,
        "an unchanged slot must be a no-op refresh",
    );

    backend.destroy(restored).await.expect("destroy restored");
}
