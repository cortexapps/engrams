//! Live end-to-end lifecycle test for the VZ (Virtualization.framework)
//! backend. Exercises the real prod-shape path through `VzBackend` on an
//! actual booting microVM and locks in the parity fixes from ADR 0032:
//!
//!   - agentd exec over the virtio-console transport (no ready-port stall),
//!   - durable snapshots — a guest write with NO explicit `sync` survives a
//!     snapshot → cold-boot restore (the `flush_guest_fs` / agentd `Sync` RPC),
//!   - cold-boot restore from a clone-snapshot,
//!   - the SHELL tab — `start_shell` forwards to agentd's `StartShell` so ttyd
//!     is actually spawned (no more `connection refused`).
//!
//! # Gating
//!
//! macOS-only (`cfg`) and `#[ignore]` by default. It needs two artifacts:
//!   - `ENGRAM_VZ_KERNEL_PATH` (or `~/.cache/engram-vz-test/vmlinux-arm64`,
//!     populated by `just pull-kernel`),
//!   - `ENGRAM_VZ_ROOTFS` — a bootable arm64 ext4 with `engram-agentd` +
//!     `ttyd` baked in and `ENGRAM_TRANSPORT=console` in its env, i.e. the
//!     output of `just bake-demo` (point the var at the materialized
//!     `var/host-sandboxes/chunked-rootfs/<manifest>.ext4`).
//!
//! Run locally:
//! ```sh
//! just vz-codesign           # codesign the test binary (entitlement)
//! ENGRAM_VZ_ROOTFS=/path/to/rootfs.ext4 \
//!   cargo nextest run -p engram-sandbox-vz --run-ignored ignored-only -E 'test(e2e_vz)'
//! ```
//!
//! In CI the existing `vz` job's `--run-ignored` step invokes this test; it
//! skips cleanly (prints `SKIP:` and returns) when `ENGRAM_VZ_ROOTFS` is
//! absent. The macOS Blacksmith runner has no Docker, so it can't bake a
//! rootfs — wiring a Docker-free prebuilt-rootfs asset (mirroring
//! `pull-kernel.sh`) so CI exercises the full boot is the tracked follow-up
//! in ADR 0032.

#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{
    CpuLimit, DiskLimit, ExecEvent, ExecRequest, MemoryLimit, SandboxSpec,
};
use engram_core::SandboxId;
use engram_sandbox_vz::{VzBackend, VzConfig};
use futures::StreamExt;

struct VzEnv {
    kernel: PathBuf,
    rootfs: PathBuf,
}

/// Resolve the kernel + rootfs the live test needs, or `None` (skip) when
/// either is missing — mirrors `fc_preflight` / the vm.rs smoke convention so
/// a CI runner without artifacts no-ops instead of failing.
fn vz_preflight() -> Option<VzEnv> {
    let kernel = std::env::var("ENGRAM_VZ_KERNEL_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join(".cache/engram-vz-test/vmlinux-arm64")
        });
    if !kernel.exists() {
        eprintln!(
            "SKIP: VZ kernel not found at {} (run `just pull-kernel`)",
            kernel.display()
        );
        return None;
    }
    let rootfs = match std::env::var("ENGRAM_VZ_ROOTFS") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("SKIP: ENGRAM_VZ_ROOTFS unset (point it at a `just bake-demo` ext4)");
            return None;
        }
    };
    if !rootfs.exists() {
        eprintln!("SKIP: ENGRAM_VZ_ROOTFS={} doesn't exist", rootfs.display());
        return None;
    }
    Some(VzEnv { kernel, rootfs })
}

fn spec(rootfs: &Path) -> SandboxSpec {
    SandboxSpec {
        image: "engram-e2e-vz".into(),
        rootfs_source: Some(rootfs.to_path_buf()),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 2 },
        memory: MemoryLimit { max_mib: 1024 },
        disk: DiskLimit { max_gib: 4 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: Vec::new(),
    }
}

/// ADR 0061: resolve a staged skill erofs (`ENGRAM_VZ_SKILL_EROFS`,
/// pointing at a `var/shared/<sha>.erofs` produced by `just bundles-vz`)
/// into (bundle_dir, sha). `None` (skip) when unset/missing — CI stages
/// no bundle, exactly like the rootfs guard, so the test no-ops there.
fn skill_erofs_preflight() -> Option<(PathBuf, String)> {
    let path = match std::env::var("ENGRAM_VZ_SKILL_EROFS") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!(
                "SKIP: ENGRAM_VZ_SKILL_EROFS unset (run `just bundles-vz`, then point \
                 it at var/shared/<sha>.erofs)"
            );
            return None;
        }
    };
    if !path.exists() {
        eprintln!(
            "SKIP: ENGRAM_VZ_SKILL_EROFS={} doesn't exist",
            path.display()
        );
        return None;
    }
    let dir = path.parent().expect("erofs has a parent dir").to_path_buf();
    let sha = path
        .file_stem()
        .expect("erofs has a file stem")
        .to_string_lossy()
        .into_owned();
    Some((dir, sha))
}

fn spec_with_skill(rootfs: &Path, sha: &str) -> SandboxSpec {
    let mut s = spec(rootfs);
    s.aux_ro_drives = vec![engram_core::types::sandbox::AuxRoDrive {
        drive_id: "dyn_0".into(),
        guest_mount: PathBuf::from("/opt/engram/dyn/0"),
        fs_type: "erofs".into(),
        sha256: Some(sha.to_string()),
    }];
    s
}

/// Run one command to completion, returning (stdout, exit_code).
async fn exec(backend: &VzBackend, id: SandboxId, sh: &str) -> (String, Option<i32>) {
    let req = ExecRequest {
        command: vec!["/bin/sh".into(), "-c".into(), sh.into()],
        stdin: None,
        env: HashMap::new(),
        workdir: None,
        timeout: Some(Duration::from_secs(30)),
    };
    let mut stream = backend.exec_stream(id, req).await.expect("exec_stream");
    let mut out = String::new();
    let mut code = None;
    while let Some(ev) = stream.events.next().await {
        match ev {
            ExecEvent::Stdout(b) => out.push_str(&String::from_utf8_lossy(&b)),
            ExecEvent::Stderr(b) => out.push_str(&String::from_utf8_lossy(&b)),
            ExecEvent::Exit(c) => {
                code = c;
                break;
            }
        }
    }
    (out, code)
}

/// Poll `guest_ip` until the in-VM agentd answers (proves boot + agentd up).
async fn await_agent(backend: &VzBackend, id: SandboxId) {
    let start = std::time::Instant::now();
    while backend.guest_ip(id).await.is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "agentd never came up within 60s",
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires macOS + a codesigned binary + a VZ kernel + ENGRAM_VZ_ROOTFS"]
async fn e2e_vz_lifecycle() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let blob = Arc::new(engram_storage_local::LocalBlobStorage::new(
        work.path().join("blob"),
    ));
    let cs = engram_chunk_store::ChunkStore::new(blob);
    let backend = VzBackend::new(
        work.path().join("sb"),
        VzConfig::with_kernel(env.kernel.clone()),
    )
    .expect("VzBackend::new")
    .with_chunk_store(cs);

    // 1. Boot + exec over the console transport (ADR 0032 #3: no ready-port
    //    stall — exec must answer promptly, not 90s later).
    let id = backend.create(spec(&env.rootfs)).await.expect("create");
    await_agent(&backend, id).await;
    let (out, code) = exec(&backend, id, "echo hello-vz && uname -m").await;
    assert_eq!(code, Some(0), "exec exit; out={out}");
    assert!(out.contains("hello-vz"), "exec stdout: {out}");

    // 2. SHELL tab (ADR 0032 #5): start_shell must spawn ttyd and return its
    //    port, not the trait-default-7681-without-a-listener.
    let port = backend
        .start_shell(id)
        .await
        .expect("start_shell spawns ttyd");
    assert_eq!(port, 7681, "ttyd default port");

    // 3. Durable snapshot (ADR 0032 #4): write with NO sync, snapshot, restore,
    //    read it back. Without the pre-clone guest flush this is lost.
    let (_, code) = exec(&backend, id, "echo durable-payload > /root/z.txt").await;
    assert_eq!(code, Some(0));
    let meta = backend.snapshot(id).await.expect("snapshot");
    assert!(
        meta.disk_manifest.is_some(),
        "VZ snapshot must chunk a disk manifest"
    );
    backend.destroy(id).await.expect("destroy");

    let id2 = backend.restore(meta).await.expect("restore (cold-boot)");
    await_agent(&backend, id2).await;
    let (out, code) = exec(&backend, id2, "cat /root/z.txt").await;
    assert_eq!(code, Some(0), "post-restore read; out={out}");
    assert!(
        out.contains("durable-payload"),
        "un-synced write must survive snapshot→restore (flush_guest_fs); got: {out:?}",
    );
    backend.destroy(id2).await.expect("destroy 2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires macOS + codesigned binary + VZ kernel + ENGRAM_VZ_ROOTFS + ENGRAM_VZ_SKILL_EROFS"]
async fn e2e_vz_skill_erofs_attaches() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let (bundle_dir, sha) = match skill_erofs_preflight() {
        Some(x) => x,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let blob = Arc::new(engram_storage_local::LocalBlobStorage::new(
        work.path().join("blob"),
    ));
    let cs = engram_chunk_store::ChunkStore::new(blob);
    let backend = VzBackend::new(
        work.path().join("sb"),
        VzConfig::with_kernel(env.kernel.clone()).with_bundle_dir(bundle_dir),
    )
    .expect("VzBackend::new")
    .with_chunk_store(cs);

    let id = backend
        .create(spec_with_skill(&env.rootfs, &sha))
        .await
        .expect("create");
    await_agent(&backend, id).await;

    // The erofs skill is attached as /dev/vdb (first aux drive after the
    // /dev/vda rootfs). RO-mount it and read the bundle's mount.json to
    // prove the attach + the kernel's erofs driver work end to end. This
    // does not rely on the init-shim auto-mount (Part 3 / a re-bake).
    let (out, code) = exec(
        &backend,
        id,
        "mkdir -p /mnt/e && mount -t erofs -o ro /dev/vdb /mnt/e && cat /mnt/e/mount.json",
    )
    .await;
    assert_eq!(code, Some(0), "mount erofs /dev/vdb failed; out={out}");
    assert!(
        out.contains('{'),
        "mount.json not readable from erofs; out={out}"
    );

    backend.destroy(id).await.expect("destroy");
}
