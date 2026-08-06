//! ADR 0112: ephemeral swap drive lifecycle against a real Firecracker.
//!
//! One VM, minimal sizes, three properties:
//! 1. A spec with `swap_mib` boots with exactly one WRITABLE non-vda
//!    virtio disk of exactly that size (the guest-side identity probe
//!    agentd's arming uses — aux drives attach read-only).
//! 2. `snapshot()` succeeds AFTER the backing file was unlinked at
//!    create (unlink-after-attach: FC must never reopen a drive by
//!    path — this assertion is the CI leg of the ADR's dev-VM probe).
//! 3. A restore of that snapshot sees a FRESH zero-filled device: a
//!    marker written to the swap device before capture must NOT be
//!    readable in the restored guest (its backing is a new sparse
//!    file re-pointed under the source canonical).
//!
//! Same gating as the other FC integration tests: Linux + KVM +
//! firecracker + a static busybox; `#[ignore]`d and wired into
//! ci.yml's `--test` list.

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_rootfs_materializer::{InitInjection, Transport};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};

use common::{drain, fc_preflight, require_bin};

const SWAP_MIB: u32 = 64;

/// Probe script: emit `<dev> <ro> <size-sectors>` for every virtio
/// disk. agentd's arming logic uses the same writable-non-vda rule.
const PROBE: &str = "for d in /sys/block/vd*; do \
     echo \"$(basename $d) $(cat $d/ro) $(cat $d/size)\"; done";

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; bakes a rootfs and boots a microVM"]
async fn swap_drive_boots_writable_and_restores_fresh() {
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
    let target_root = Path::new(&manifest).join("..").join("..").join("target");
    let agent = target_root
        .join("x86_64-unknown-linux-musl")
        .join("release")
        .join("engram-agentd");
    if !agent.exists() {
        eprintln!(
            "SKIP: static-musl engram-agentd not built at {} (run \
             scripts/run-boot-test.sh swap_disk)",
            agent.display(),
        );
        return;
    }

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

    let work = tempfile::tempdir().expect("work dir");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agent);
    cfg.bundle_dir = staged.bundle_dir.clone();
    cfg.net_pool = None;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
    cfg.track_dirty_pages = true;
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "engram-swap-test".into(),
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
        aux_ro_drives: vec![staged.agentd_slot()],
        swap_mib: Some(SWAP_MIB),
    };
    let original_id = backend.create(spec).await.expect("create");
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    // Property 0 (host side): the backing file was unlinked after
    // attach — nothing under <work_dir>/swap/ but the canonical symlink.
    let swap_dir = work.path().join("swap");
    let entries: Vec<_> = std::fs::read_dir(&swap_dir)
        .expect("swap dir exists")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries.iter().all(|n| n.ends_with(".swap")),
        "swap backing file survived unlink-after-attach: {entries:?}",
    );

    // Property 1: the guest sees exactly one writable non-vda disk of
    // SWAP_MIB (512-byte sectors).
    let probe_out = exec_ok(&backend, original_id, PROBE, Duration::from_secs(20)).await;
    let swap_dev = parse_swap_device(&probe_out);
    let expected_sectors = u64::from(SWAP_MIB) * 2048;
    assert_eq!(
        swap_dev.1, expected_sectors,
        "swap device size mismatch (probe: {probe_out})",
    );

    // Write a marker into the swap device, prove it reads back live.
    let dev = swap_dev.0;
    let marked = exec_ok(
        &backend,
        original_id,
        &format!(
            "printf SWAPMARK | dd of=/dev/{dev} conv=notrunc 2>/dev/null && \
             dd if=/dev/{dev} bs=8 count=1 2>/dev/null"
        ),
        Duration::from_secs(10),
    )
    .await;
    assert!(
        marked.contains("SWAPMARK"),
        "marker did not read back on the live guest: {marked:?}",
    );

    // Property 2: snapshot succeeds with the backing long unlinked.
    let metadata = backend.snapshot(original_id).await.expect("snapshot");
    backend
        .destroy(original_id)
        .await
        .expect("destroy original");

    // Property 3: the restored guest's swap device is FRESH — the
    // marker is gone (new sparse file re-pointed under the source
    // canonical), and the device is still the right size.
    let restored_id = backend.restore(metadata).await.expect("restore");
    let probe_out = exec_ok(&backend, restored_id, PROBE, Duration::from_secs(20)).await;
    let restored_dev = parse_swap_device(&probe_out);
    assert_eq!(restored_dev.1, expected_sectors, "restored size mismatch");
    let first_bytes = exec_ok(
        &backend,
        restored_id,
        &format!(
            "dd if=/dev/{} bs=8 count=1 2>/dev/null | od -c | head -1",
            restored_dev.0
        ),
        Duration::from_secs(10),
    )
    .await;
    assert!(
        !first_bytes.contains("S   W   A   P"),
        "restored swap device leaked the source residence's bytes: {first_bytes:?}",
    );
    assert!(
        first_bytes.contains("\\0") || first_bytes.contains("0000000"),
        "expected zero-filled fresh device, got: {first_bytes:?}",
    );

    backend
        .destroy(restored_id)
        .await
        .expect("destroy restored");

    // Property 4 (review finding on #1051): destroy leaves no `.img`
    // BACKING in the swap dir — that's the leak class (plaintext guest
    // swap bytes). The SOURCE canonical symlink (`swap/<ancestor>.swap`,
    // re-installed by the restore at the state.bin-embedded path) is
    // deliberately allowed to remain: it is shared by every same-base
    // descendant — removing it at one descendant's destroy would race a
    // sibling restore between symlink-install and FC's open (the ADR
    // 0048 class) — exactly the rootfs source-canonical lifecycle. The
    // startup residue sweep reaps it with the lineage's other canonical
    // entries once the ids are dead.
    let leaked_backings: Vec<_> = std::fs::read_dir(&swap_dir)
        .map(|it| {
            it.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".img"))
                .collect()
        })
        .unwrap_or_default();
    assert!(
        leaked_backings.is_empty(),
        "no swap backing may survive destroy: {leaked_backings:?}",
    );
}

/// Parse the probe output into the single expected writable non-vda
/// device: `(name, size_sectors)`. Panics (with the probe output) if
/// zero or several match — both are wiring bugs worth failing loudly.
fn parse_swap_device(probe: &str) -> (String, u64) {
    let candidates: Vec<(String, u64)> = probe
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let (name, ro, size) = (it.next()?, it.next()?, it.next()?);
            (ro == "0" && name != "vda").then(|| (name.to_string(), size.parse().unwrap_or(0)))
        })
        .collect();
    assert_eq!(
        candidates.len(),
        1,
        "expected exactly one writable non-vda disk, probe said:\n{probe}",
    );
    candidates.into_iter().next().unwrap()
}

/// Exec a shell command, polling until agentd accepts (boot / resume
/// settle), and return stdout. Panics on nonzero exit.
async fn exec_ok(
    backend: &FirecrackerBackend,
    id: engram_core::types::ids::SandboxId,
    sh: &str,
    budget: Duration,
) -> String {
    let req = ExecRequest {
        command: vec!["/bin/sh".into(), "-c".into(), sh.into()],
        stdin: None,
        env: HashMap::new(),
        workdir: None,
        timeout: Some(Duration::from_secs(10)),
        exec_id: None,
        stdout_offset: None,
        stderr_offset: None,
        wake: None,
    };
    let deadline = std::time::Instant::now() + budget;
    let mut last_err = None;
    let stream = loop {
        match backend.exec_stream(id, req.clone()).await {
            Ok(s) => break s,
            Err(e) if std::time::Instant::now() < deadline => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => panic!("agent never came up: {e:?} (last: {last_err:?})"),
        }
    };
    let (stdout, stderr, exit) = drain(stream.events).await;
    assert_eq!(
        exit,
        Some(0),
        "exec `{sh}` failed (stderr: {})",
        String::from_utf8_lossy(&stderr),
    );
    String::from_utf8_lossy(&stdout).into_owned()
}
