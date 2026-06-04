//! ADR 0028 P1 spike + permanent regression gate: **diff snapshots on
//! our FC build**, end-to-end through the exact mechanics the periodic
//! checkpoint pipeline (Fix A) is designed around:
//!
//!   1. Cold-boot a VM with `track_dirty_pages: true`, plant an 8 MiB
//!      in-RAM marker (tmpfs), take a **Full** capture — the chain
//!      baseline.
//!   2. Plant a second marker, take a **Diff** capture
//!      (`SnapshotType::Diff`) — assert it's sparse (allocated bytes ≪
//!      guest RAM) and time the pause window.
//!   3. **Rebase**: overlay the diff's data extents onto a copy of the
//!      Full memory.bin (the "rolling memory.bin" of ADR 0028), restore
//!      from (diff state.bin + rebased memory) and verify *both* markers
//!      via sha256 — proving the (capture → overlay → restore) loop is
//!      byte-faithful.
//!   4. **Chained diff after a restore**: the restored VM re-armed dirty
//!      tracking via `enable_diff_snapshots` at `snapshot/load`
//!      (`FirecrackerConfig::track_dirty_pages`); plant a third marker,
//!      Diff again, overlay onto the same rolling file, restore, verify
//!      all three markers. This is the steady-state checkpoint chain:
//!      every prod VM is a restore, so diff-after-load MUST work.
//!
//! Timings + sizes print as `SPIKE:` lines (`--nocapture`) and feed ADR
//! 0028's "Status / phase chain" P1 numbers.
//!
//! Same gating as `exec_real_vm.rs` (Linux + KVM + firecracker + Docker
//! + mke2fs + musl agentd). Run via:
//!
//! ```sh
//! bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh diff_snapshot
//! ```

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::ids::SnapshotId;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_image_builder::{AgentInjection, BuildRequest, Builder, DockerCli, Format};
use engram_sandbox_firecracker::client::{FirecrackerClient, SnapshotType};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};

use common::{drain, fc_preflight, require_bin};

/// Guest RAM. Big enough that an 8 MiB marker is a small fraction —
/// the sparseness assertion needs headroom between "dirty set" and
/// "all of RAM".
const GUEST_MIB: u32 = 256;

/// Per-marker size planted in guest tmpfs (RAM).
const MARKER_BYTES: usize = 8 * 1024 * 1024;

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; bakes a rootfs and boots microVMs"]
async fn diff_snapshot_chain_rebases_and_restores_faithfully() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_bin("docker") || !require_bin("mke2fs") {
        return;
    }
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target_root = Path::new(&manifest).join("..").join("..").join("target");
    let agent = target_root
        .join("x86_64-unknown-linux-musl")
        .join("release")
        .join("engram-agentd");
    if !agent.exists() {
        eprintln!(
            "SKIP: static-musl engram-agentd not built at {}.\n  \
             Run via the script — it builds the binary fresh:\n    \
             bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh diff_snapshot",
            agent.display(),
        );
        return;
    }

    // ---- 1. Bake an agentd-injected ext4 (same shape as exec_real_vm) ----
    let src = tempfile::tempdir().expect("source dir");
    std::fs::write(src.path().join("Dockerfile"), "FROM debian:bookworm-slim\n").unwrap();
    std::fs::write(
        src.path().join("engram.toml"),
        "name = \"engram-diff-snapshot-test\"\n",
    )
    .unwrap();
    let images = tempfile::tempdir().expect("images dir");
    let chunk_root = tempfile::tempdir().expect("chunk store root");
    let blob: std::sync::Arc<dyn engram_core::traits::BlobStorage> = std::sync::Arc::new(
        engram_storage_local::LocalBlobStorage::new(chunk_root.path().to_path_buf()),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);
    let baker = Builder::new(DockerCli::new(), chunk_store);
    let outcome = baker
        .build(&BuildRequest {
            source: src.path().to_path_buf(),
            repo: "engram-diff-snapshot-test".into(),
            tag: "warm-1".into(),
            images_dir: images.path().to_path_buf(),
            format: Format::Ext4,
            agent_injection: Some(AgentInjection {
                agent_binary: agent,
                vsock_port: ENGRAM_AGENTD_PORT,
                transport: engram_image_builder::Transport::Vsock,
                init_script: None,
            }),
        })
        .await
        .expect("ext4 bake with agent injection");

    // ---- 2. Boot with dirty tracking armed ----
    let work = tempfile::tempdir().expect("work dir");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = None; // unprivileged test — see lifecycle.rs
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
    // ADR 0028: cold creates arm KVM dirty tracking via machine-config;
    // restores re-arm it via enable_diff_snapshots at snapshot/load.
    cfg.track_dirty_pages = true;
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "engram-diff-snapshot-test".into(),
        rootfs_source: Some(outcome.rootfs_path),
        image_uri: None,
        cpu: CpuLimit { vcpus: 1 },
        memory: MemoryLimit { max_mib: GUEST_MIB },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
    };
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");
    let vm1 = backend.create(spec).await.expect("create");

    // ---- 3. Marker 1 in guest RAM, then the Full baseline capture ----
    let sum1 = plant_marker(&backend, vm1, 1).await;

    let t = Instant::now();
    let metadata = backend.snapshot(vm1).await.expect("full snapshot");
    let full_ms = t.elapsed().as_millis();
    let full_dir = backend.snapshot_path_for(metadata.id);
    let full_mem_len = std::fs::metadata(full_dir.join("memory.bin"))
        .expect("memory.bin")
        .len();
    eprintln!("SPIKE: full capture {full_ms} ms, memory.bin {full_mem_len} bytes");

    // ---- 4. Marker 2, then the Diff capture ----
    let sum2 = plant_marker(&backend, vm1, 2).await;

    let st1 = backend.snapshot_state(vm1).expect("vm1 state");
    let api1 = FirecrackerClient::new(&st1.firecracker_socket);
    let diff1_dir = tempfile::tempdir().expect("diff1 dir");
    let t = Instant::now();
    api1.create_snapshot_at(
        diff1_dir.path().join("state.bin"),
        diff1_dir.path().join("memory.diff"),
        SnapshotType::Diff,
    )
    .await
    .expect("diff snapshot (cold-boot dirty tracking)");
    let diff1_ms = t.elapsed().as_millis();

    let diff1_alloc = allocated_bytes(&diff1_dir.path().join("memory.diff"));
    eprintln!(
        "SPIKE: diff capture {diff1_ms} ms (≈ pause window), {diff1_alloc} bytes allocated \
         of {full_mem_len} logical ({}%)",
        diff1_alloc * 100 / full_mem_len,
    );
    // The diff holds the dirty set since the Full capture (marker 2 +
    // guest churn), not all of RAM. Lenient bound: half. A non-sparse
    // writer (or broken dirty tracking re-dumping everything) fails it.
    assert!(
        diff1_alloc < full_mem_len / 2,
        "diff snapshot is not meaningfully sparse: {diff1_alloc} of {full_mem_len} bytes",
    );

    backend.destroy(vm1).await.expect("destroy vm1");

    // ---- 5. Rebase diff1 onto the rolling memory file; restore ----
    // This is ADR 0028's rolling-memory.bin move: checkpoint vN+1 =
    // previous full image with the diff's extents overlaid. The diff's
    // OWN state.bin pairs with the rebased memory (the vmstate of the
    // diff-capture instant).
    let meta2 = stage_rebased_snapshot(
        &backend,
        &metadata,
        &full_dir.join("memory.bin"),
        &diff1_dir.path().join("state.bin"),
        &[diff1_dir.path().join("memory.diff")],
        &full_dir.join("manifest.json"),
    );
    // The rolling memory file vm2 will map — diff2 overlays onto it
    // below, after vm2 is destroyed.
    let rebased_mem = backend.snapshot_path_for(meta2.id).join("memory.bin");
    let t = Instant::now();
    let vm2 = backend
        .restore(meta2)
        .await
        .expect("restore from rebased diff1");
    eprintln!("SPIKE: restore(full+diff1) {} ms", t.elapsed().as_millis());

    let out = exec(
        &backend,
        vm2,
        "sha256sum /dev/shm/marker1 /dev/shm/marker2 | cut -d' ' -f1",
    )
    .await;
    let sums: Vec<&str> = out.split_whitespace().collect();
    assert_eq!(
        sums,
        vec![sum1.as_str(), sum2.as_str()],
        "markers corrupted across diff+rebase restore",
    );

    // ---- 6. Chained diff AFTER a restore (the steady-state shape) ----
    // vm2 was loaded with enable_diff_snapshots=true (cfg knob), so its
    // dirty bitmap tracks divergence from the *restored* image — the
    // rebased file is the new baseline, exactly like a prod checkpoint
    // chain continuing across an idle-resume.
    let sum3 = plant_marker(&backend, vm2, 3).await;

    let st2 = backend.snapshot_state(vm2).expect("vm2 state");
    let api2 = FirecrackerClient::new(&st2.firecracker_socket);
    let diff2_dir = tempfile::tempdir().expect("diff2 dir");
    let t = Instant::now();
    api2.create_snapshot_at(
        diff2_dir.path().join("state.bin"),
        diff2_dir.path().join("memory.diff"),
        SnapshotType::Diff,
    )
    .await
    .expect("diff snapshot after restore (enable_diff_snapshots path)");
    let diff2_ms = t.elapsed().as_millis();
    let diff2_alloc = allocated_bytes(&diff2_dir.path().join("memory.diff"));
    eprintln!("SPIKE: post-restore diff capture {diff2_ms} ms, {diff2_alloc} bytes allocated");
    assert!(
        diff2_alloc < full_mem_len / 2,
        "post-restore diff is not sparse — enable_diff_snapshots at load \
         did not arm dirty tracking ({diff2_alloc} of {full_mem_len} bytes)",
    );

    // Destroy BEFORE overlaying: vm2 has the rebased file MAP_PRIVATE'd;
    // mutating a live mapping's backing file corrupts unfaulted pages.
    backend.destroy(vm2).await.expect("destroy vm2");

    let meta3 = stage_rebased_snapshot(
        &backend,
        &metadata,
        &rebased_mem,
        &diff2_dir.path().join("state.bin"),
        &[diff2_dir.path().join("memory.diff")],
        &full_dir.join("manifest.json"),
    );
    let vm3 = backend
        .restore(meta3)
        .await
        .expect("restore from chained diff2");
    let out = exec(
        &backend,
        vm3,
        "sha256sum /dev/shm/marker1 /dev/shm/marker2 /dev/shm/marker3 | cut -d' ' -f1",
    )
    .await;
    let sums: Vec<&str> = out.split_whitespace().collect();
    assert_eq!(
        sums,
        vec![sum1.as_str(), sum2.as_str(), sum3.as_str()],
        "markers corrupted across the chained diff restore",
    );

    backend.destroy(vm3).await.expect("destroy vm3");
}

/// Write `MARKER_BYTES` of urandom to `/dev/shm/marker<n>` (guest RAM)
/// and return its sha256.
async fn plant_marker(
    backend: &FirecrackerBackend,
    id: engram_core::types::ids::SandboxId,
    n: u32,
) -> String {
    let out = exec(
        backend,
        id,
        &format!(
            "head -c {MARKER_BYTES} /dev/urandom > /dev/shm/marker{n} \
             && sha256sum /dev/shm/marker{n} | cut -d' ' -f1"
        ),
    )
    .await;
    let sum = out.trim().to_string();
    assert_eq!(sum.len(), 64, "expected a sha256, got {sum:?}");
    sum
}

/// Exec `sh -c <cmd>` in the guest (polling until agentd is reachable —
/// fresh boots and restores both need a beat) and return stdout.
async fn exec(
    backend: &FirecrackerBackend,
    id: engram_core::types::ids::SandboxId,
    cmd: &str,
) -> String {
    let req = ExecRequest {
        command: vec!["/bin/sh".into(), "-c".into(), cmd.into()],
        stdin: None,
        env: HashMap::new(),
        workdir: None,
        timeout: Some(Duration::from_secs(30)),
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_err = None;
    let stream = loop {
        match backend.exec_stream(id, req.clone()).await {
            Ok(s) => break s,
            Err(e) if Instant::now() < deadline => {
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
        "guest cmd failed: {cmd}\nstderr: {}",
        String::from_utf8_lossy(&stderr),
    );
    String::from_utf8_lossy(&stdout).into_owned()
}

/// Stage a restorable snapshot dir: rolling memory = `base_mem` with
/// each diff's data extents overlaid, paired with the diff-instant
/// `state_bin` and the lineage's `manifest.json`. Returns metadata
/// whose id points at the staged dir.
fn stage_rebased_snapshot(
    backend: &FirecrackerBackend,
    lineage: &engram_core::types::snapshot::SnapshotMetadata,
    base_mem: &Path,
    state_bin: &Path,
    diffs: &[PathBuf],
    manifest_json: &Path,
) -> engram_core::types::snapshot::SnapshotMetadata {
    let id = SnapshotId::new();
    let dir = backend.snapshot_path_for(id);
    std::fs::create_dir_all(&dir).expect("staged snapshot dir");
    std::fs::copy(base_mem, dir.join("memory.bin")).expect("copy rolling memory");
    let mut total = 0u64;
    for d in diffs {
        total += overlay_sparse(d, &dir.join("memory.bin")).expect("overlay diff");
    }
    eprintln!(
        "SPIKE: rebase overlaid {total} bytes onto {}",
        dir.join("memory.bin").display()
    );
    std::fs::copy(state_bin, dir.join("state.bin")).expect("copy state.bin");
    std::fs::copy(manifest_json, dir.join("manifest.json")).expect("copy manifest.json");
    engram_core::types::snapshot::SnapshotMetadata {
        id,
        ..lineage.clone()
    }
}

/// Copy `diff`'s data extents (skipping holes) onto `base` at the same
/// offsets — the userspace half of FC's documented diff-rebase. Returns
/// bytes copied.
fn overlay_sparse(diff: &Path, base: &Path) -> std::io::Result<u64> {
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::os::unix::io::AsRawFd;

    let mut src = std::fs::File::open(diff)?;
    let mut dst = std::fs::OpenOptions::new().write(true).open(base)?;
    let len = src.metadata()?.len() as i64;
    let fd = src.as_raw_fd();
    let mut buf = vec![0u8; 1 << 20];
    let mut copied = 0u64;
    let mut off: i64 = 0;
    while off < len {
        let data = unsafe { libc::lseek(fd, off, libc::SEEK_DATA) };
        if data < 0 {
            let errno = std::io::Error::last_os_error();
            if errno.raw_os_error() == Some(libc::ENXIO) {
                break; // no more data extents
            }
            return Err(errno);
        }
        let hole = unsafe { libc::lseek(fd, data, libc::SEEK_HOLE) };
        let hole = if hole < 0 { len } else { hole };
        src.seek(SeekFrom::Start(data as u64))?;
        dst.seek(SeekFrom::Start(data as u64))?;
        let mut remaining = (hole - data) as u64;
        while remaining > 0 {
            let n = remaining.min(buf.len() as u64) as usize;
            src.read_exact(&mut buf[..n])?;
            dst.write_all(&buf[..n])?;
            remaining -= n as u64;
            copied += n as u64;
        }
        off = hole;
    }
    Ok(copied)
}

/// Bytes actually allocated on disk (sparse-aware), vs logical length.
fn allocated_bytes(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).expect("metadata").blocks() * 512
}
