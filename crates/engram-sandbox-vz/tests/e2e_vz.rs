//! Live end-to-end tests for the VZ backend on a real booting microVM:
//! agentd exec over vsock, the boot contract (agentd comes from its
//! reserved bundle slot, resolved from the staged `current.json`),
//! snapshot and warm or cold restore of caller-provided disks, the SHELL
//! tab, the port relay, egress steering, crash detection, pause and
//! resume, the forge credential broker, and the browser bundle.
//!
//! # Gating
//!
//! macOS-only and `#[ignore]` by default. `just vz-e2e` stages everything
//! and runs the suite. The env contract:
//!
//!   - `ENGRAM_VZ_KERNEL_PATH` (or `~/.cache/engram-vz-test/vmlinux-arm64`,
//!     populated by `just pull-kernel`),
//!   - `ENGRAM_VZ_ROOTFS`: a bootable arm64 ext4 whose init is the stage-1
//!     shim (`make-test-rootfs.sh`, or any materialized session image);
//!     agentd itself must NOT be baked in,
//!   - `ENGRAM_VZ_BUNDLE_DIR`: a staged bundle dir (`<sha>.squashfs` files
//!     and a `current.json` with at least the `agentd` and `guest-tools`
//!     keys; `just vz-test-bundles` produces the minimal set),
//!   - `ENGRAM_VZ_REQUIRE=1` (optional): every preflight SKIP becomes a
//!     hard failure. CI sets it so the lane cannot pass vacuously.
//!
//! The skill and browser variants also need `ENGRAM_VZ_SKILL_SQUASHFS` (a
//! Docker-built bundle staged into the same dir as `ENGRAM_VZ_BUNDLE_DIR`)
//! and a glibc rootfs for the browser stack; they stay soft skips even
//! under `ENGRAM_VZ_REQUIRE`.

#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{
    AuxRoDrive, CpuLimit, DiskLimit, ExecEvent, ExecRequest, MemoryLimit, SandboxSpec, SessionDisks,
};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::{SandboxError, SandboxId};
use engram_harness_proto::{
    read_msg, write_msg, ForgeOp, ForgeRequest, ForgeResponse, RelayAck, RelayConnect,
    PROXY_PORT_VSOCK_PORT,
};
use engram_sandbox_vz::{VzBackend, VzConfig};
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct VzEnv {
    kernel: PathBuf,
    rootfs: PathBuf,
    bundle_dir: PathBuf,
}

/// A failed preflight is a clean SKIP locally and a hard failure under
/// `ENGRAM_VZ_REQUIRE=1`.
fn skip(msg: &str) {
    if std::env::var("ENGRAM_VZ_REQUIRE").as_deref() == Ok("1") {
        panic!("ENGRAM_VZ_REQUIRE=1: {msg}");
    }
    eprintln!("SKIP: {msg}");
}

/// Resolve the kernel + rootfs + bundle dir the live test needs, or `None`
/// (skip; panic under `ENGRAM_VZ_REQUIRE=1`).
fn vz_preflight() -> Option<VzEnv> {
    let kernel = std::env::var("ENGRAM_VZ_KERNEL_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join(".cache/engram-vz-test/vmlinux-arm64")
        });
    if !kernel.exists() {
        skip(&format!(
            "VZ kernel not found at {} (run `just pull-kernel`)",
            kernel.display()
        ));
        return None;
    }
    let rootfs = match std::env::var("ENGRAM_VZ_ROOTFS") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            skip("ENGRAM_VZ_ROOTFS unset (run `just vz-e2e`, which stages a fresh one)");
            return None;
        }
    };
    if !rootfs.exists() {
        skip(&format!(
            "ENGRAM_VZ_ROOTFS={} doesn't exist",
            rootfs.display()
        ));
        return None;
    }
    let bundle_dir = match std::env::var("ENGRAM_VZ_BUNDLE_DIR") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            skip("ENGRAM_VZ_BUNDLE_DIR unset (run `just vz-test-bundles`)");
            return None;
        }
    };
    if !bundle_dir.join(AuxRoDrive::CURRENT_STAMP).exists() {
        skip(&format!(
            "no {} in ENGRAM_VZ_BUNDLE_DIR={} (run `just vz-test-bundles`)",
            AuxRoDrive::CURRENT_STAMP,
            bundle_dir.display()
        ));
        return None;
    }
    Some(VzEnv {
        kernel,
        rootfs,
        bundle_dir,
    })
}

/// Host-side tracing lands in the captured stderr, next to the guest
/// console, so a failed run shows both sides.
fn backend(env: &VzEnv, work: &Path) -> VzBackend {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_test_writer()
        .try_init();
    VzBackend::new(
        work.join("sb"),
        VzConfig::with_kernel(env.kernel.clone()).with_bundle_dir(env.bundle_dir.clone()),
    )
    .expect("VzBackend::new")
}

/// The disk owner's job, emulated: each sandbox boots its own writable
/// copy of the staged rootfs (an APFS clone, through `fs::copy`).
fn session_rootfs(env: &VzEnv, work: &Path, id: SandboxId) -> PathBuf {
    let path = work.join(format!("{id}.ext4"));
    std::fs::copy(&env.rootfs, &path).expect("copy rootfs");
    path
}

fn disks(root: &Path) -> Option<SessionDisks> {
    Some(SessionDisks {
        root: root.to_path_buf(),
        swap: None,
    })
}

/// The disk owner's capture protocol, emulated: flush the guest fs,
/// pause, copy the disk, save the machine state, resume. Returns the
/// metadata and the disk copy a restore attaches.
async fn snapshot_with_disk(
    b: &VzBackend,
    id: SandboxId,
    rootfs: &Path,
    work: &Path,
) -> (SnapshotMetadata, PathBuf) {
    let (_, code) = exec(b, id, "sync").await;
    assert_eq!(code, Some(0), "guest sync");
    b.pause(id).await.expect("pause");
    let copy = work.join(format!("snap-{id}.ext4"));
    std::fs::copy(rootfs, &copy).expect("copy disk");
    let meta = b.snapshot(id).await.expect("snapshot");
    b.resume(id).await.expect("resume");
    (meta, copy)
}

/// Every live spec carries the symbolic agentd and guest-tools reserved
/// slots, resolved against the staged `current.json` as real sessions
/// are. The init shim panics the kernel when no agentd bundle is mounted.
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
        aux_ro_drives: vec![
            AuxRoDrive::reserved_slot(AuxRoDrive::AGENTD_SLOT_INDEX),
            AuxRoDrive::reserved_slot(AuxRoDrive::GUEST_TOOLS_SLOT_INDEX),
        ],
        swap_mib: None,
        swap_source: None,
        swap_manifest: None,
    }
}

/// Resolve `ENGRAM_VZ_SKILL_SQUASHFS` (a `<bundle_dir>/<sha>.squashfs`)
/// into its sha. A soft skip when unset or missing: the skill and browser
/// bundles are Docker-built, which the CI runner cannot produce.
fn skill_bundle_preflight(env: &VzEnv) -> Option<String> {
    let path = match std::env::var("ENGRAM_VZ_SKILL_SQUASHFS") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!(
                "SKIP: ENGRAM_VZ_SKILL_SQUASHFS unset (run `just bundles-squashfs`, then \
                 point it at a <sha>.squashfs staged in ENGRAM_VZ_BUNDLE_DIR)"
            );
            return None;
        }
    };
    if !path.exists() {
        eprintln!(
            "SKIP: ENGRAM_VZ_SKILL_SQUASHFS={} doesn't exist",
            path.display()
        );
        return None;
    }
    if path.parent() != Some(env.bundle_dir.as_path()) {
        eprintln!(
            "SKIP: ENGRAM_VZ_SKILL_SQUASHFS={} is not staged inside ENGRAM_VZ_BUNDLE_DIR={} \
             (the backend resolves every drive against ONE bundle dir)",
            path.display(),
            env.bundle_dir.display(),
        );
        return None;
    }
    Some(
        path.file_stem()
            .expect("squashfs has a file stem")
            .to_string_lossy()
            .into_owned(),
    )
}

fn spec_with_skill(rootfs: &Path, sha: &str) -> SandboxSpec {
    let mut s = spec(rootfs);
    s.aux_ro_drives
        .push(engram_core::types::sandbox::AuxRoDrive {
            drive_id: "dyn_0".into(),
            guest_mount: PathBuf::from("/opt/engram/dyn/0"),
            fs_type: "squashfs".into(),
            sha256: Some(sha.to_string()),
        });
    s
}

/// Run one command to completion, returning (stdout, exit_code).
async fn exec(backend: &VzBackend, id: SandboxId, sh: &str) -> (String, Option<i32>) {
    let (out, exit) = try_exec(backend, id, sh).await.expect("exec_stream");
    (out, exit.flatten())
}

/// Preserve submission errors and distinguish stream loss from any Exit event.
async fn try_exec(
    backend: &VzBackend,
    id: SandboxId,
    sh: &str,
) -> Result<(String, Option<Option<i32>>), SandboxError> {
    let req = ExecRequest {
        command: vec!["/bin/sh".into(), "-c".into(), sh.into()],
        stdin: None,
        env: HashMap::new(),
        workdir: None,
        timeout: Some(Duration::from_secs(30)),
        exec_id: None,
        stdout_offset: None,
        stderr_offset: None,
        wake: None,
    };
    let mut stream = backend.exec_stream(id, req).await?;
    let mut out = String::new();
    let mut code = None;
    while let Some(ev) = stream.events.next().await {
        match ev {
            ExecEvent::Stdout(b) => out.push_str(&String::from_utf8_lossy(&b)),
            ExecEvent::Stderr(b) => out.push_str(&String::from_utf8_lossy(&b)),
            ExecEvent::Exit(c) => {
                code = Some(c);
                break;
            }
            ExecEvent::Refused(reason) => panic!("exec refused: {reason}"),
        }
    }
    Ok((out, code))
}

fn exec_has_terminal_exit(result: &Result<(String, Option<Option<i32>>), SandboxError>) -> bool {
    matches!(result, Ok((_, Some(_))))
}

#[test]
fn parked_exec_rejects_every_terminal_exit() {
    assert!(!exec_has_terminal_exit(&Err(SandboxError::Unavailable(
        "transport closed before submission".into(),
    ))));
    assert!(!exec_has_terminal_exit(&Ok((String::new(), None))));
    for code in [None, Some(0), Some(1)] {
        assert!(exec_has_terminal_exit(&Ok((String::new(), Some(code)))));
    }
}

/// Poll `guest_endpoints` until the in-VM agentd answers (proves boot + agentd up).
async fn await_agent(backend: &VzBackend, id: SandboxId) {
    let start = std::time::Instant::now();
    while backend.guest_endpoints(id).await.is_none() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "agentd never came up within 60s",
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A spec with `swap_mib` and a swap disk boots with exactly one writable
/// non-vda virtio disk of that size, and agentd arms it at boot (mkswap +
/// swapon + vm.swappiness).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot: run via `just vz-e2e` (macOS + codesigned + staged artifacts)"]
async fn e2e_vz_swap_drive_arms() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let backend = backend(&env, work.path());

    let id = SandboxId::new();
    let mut s = spec(&session_rootfs(&env, work.path(), id));
    s.swap_mib = Some(64);
    let swap = work.path().join("swap.img");
    std::fs::File::create(&swap)
        .and_then(|f| f.set_len(64 * 1024 * 1024))
        .expect("sparse swap");
    s.swap_source = Some(swap);
    backend.create(id, s).await.expect("create with swap");
    await_agent(&backend, id).await;

    // Exactly one writable non-vda disk, sized 64 MiB (131072 sectors).
    let (out, code) = exec(
        &backend,
        id,
        "for d in /sys/block/vd*; do echo \"$(basename $d) $(cat $d/ro) $(cat $d/size)\"; done",
    )
    .await;
    assert_eq!(code, Some(0), "probe exit; out={out}");
    let writable: Vec<&str> = out
        .lines()
        .filter(|l| {
            let mut it = l.split_whitespace();
            let (name, ro) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
            name != "vda" && ro == "0"
        })
        .collect();
    assert_eq!(writable.len(), 1, "one writable non-vda disk: {out}");
    assert!(
        writable[0].ends_with(" 131072"),
        "swap device size (sectors): {out}",
    );

    // agentd armed it at boot (may lag agent-up by a beat — poll).
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let (swaps, code) = exec(&backend, id, "cat /proc/swaps").await;
        if code == Some(0) && swaps.contains("/dev/vd") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "agentd never armed swap; /proc/swaps: {swaps}",
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let (swappiness, _) = exec(&backend, id, "cat /proc/sys/vm/swappiness").await;
    assert_eq!(swappiness.trim(), "100", "vm.swappiness applied");
    backend.destroy(id).await.expect("destroy");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot: run via `just vz-e2e` (macOS + codesigned + staged artifacts)"]
async fn e2e_vz_lifecycle() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let backend = backend(&env, work.path());

    // 1. Boot and exec over vsock.
    let id = SandboxId::new();
    let rootfs = session_rootfs(&env, work.path(), id);
    backend.create(id, spec(&rootfs)).await.expect("create");
    await_agent(&backend, id).await;
    let (out, code) = exec(&backend, id, "echo hello-vz && uname -m").await;
    assert_eq!(code, Some(0), "exec exit; out={out}");
    assert!(out.contains("hello-vz"), "exec stdout: {out}");

    // 2. SHELL tab: start_shell spawns ttyd from the guest-tools slot (the
    //    rootfs bakes no ttyd) and returns its port.
    let port = backend
        .start_shell(id)
        .await
        .expect("start_shell spawns ttyd");
    assert_eq!(port, 7681, "ttyd default port");

    // 3. Durable snapshot: write, snapshot, restore, read it back.
    let (_, code) = exec(&backend, id, "echo durable-payload > /root/z.txt").await;
    assert_eq!(code, Some(0));
    let (meta, disk) = snapshot_with_disk(&backend, id, &rootfs, work.path()).await;
    backend.destroy(id).await.expect("destroy");

    let id2 = SandboxId::new();
    backend
        .restore(id2, meta, disks(&disk))
        .await
        .expect("restore");
    await_agent(&backend, id2).await;
    let (out, code) = exec(&backend, id2, "cat /root/z.txt").await;
    assert_eq!(code, Some(0), "post-restore read; out={out}");
    assert!(
        out.contains("durable-payload"),
        "write must survive snapshot→restore; got: {out:?}",
    );
    backend.destroy(id2).await.expect("destroy 2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot + a Docker-built skill bundle (ENGRAM_VZ_SKILL_SQUASHFS)"]
async fn e2e_vz_skill_bundle_attaches() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let sha = match skill_bundle_preflight(&env) {
        Some(x) => x,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let backend = backend(&env, work.path());

    let id = SandboxId::new();
    backend
        .create(
            id,
            spec_with_skill(&session_rootfs(&env, work.path(), id), &sha),
        )
        .await
        .expect("create");
    await_agent(&backend, id).await;

    // Attach order is slot-ascending, so the skill (slot 0) is the first
    // aux drive: /dev/vdb (after the /dev/vda rootfs).
    // RO-mount it and read the bundle's mount.json to prove the attach +
    // the kernel's squashfs driver work end to end (a second RO mount of
    // an already-init-shim-mounted device shares the superblock — fine).
    let (out, code) = exec(
        &backend,
        id,
        "mkdir -p /mnt/e && mount -t squashfs -o ro /dev/vdb /mnt/e && cat /mnt/e/mount.json",
    )
    .await;
    assert_eq!(code, Some(0), "mount squashfs /dev/vdb failed; out={out}");
    assert!(
        out.contains('{'),
        "mount.json not readable from squashfs; out={out}"
    );

    backend.destroy(id).await.expect("destroy");
}

/// The port relay reaches a server bound to the guest's `127.0.0.1`, and
/// a persistent forwarded connection does not head-of-line block a fresh
/// one.
///
/// Uses the cross-built `vz-e2e-echo` helper `make-test-rootfs.sh` bakes
/// at /usr/bin (std-only echo + hold-open sink on guest loopback),
/// detached via `setsid` so it survives the exec returning.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot: run via `just vz-e2e` (macOS + codesigned + staged artifacts)"]
async fn e2e_vz_port_relay_reaches_loopback_without_hol() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let backend = backend(&env, work.path());

    let id = SandboxId::new();
    backend
        .create(id, spec(&session_rootfs(&env, work.path(), id)))
        .await
        .expect("create");
    await_agent(&backend, id).await;

    // Bring loopback up (the relay dials 127.0.0.1; a minimal guest may leave
    // `lo` down), then start the echo server on 127.0.0.1:ECHO and the
    // black-hole sink on 127.0.0.1:SINK (accepts but never replies — stands
    // in for a persistent HMR WebSocket / noVNC stream). Each echo
    // connection gets its own thread, so any HOL we observe is the relay's,
    // not the server's. `setsid … &` detaches it so the exec returns while
    // it keeps running (reparented to agentd).
    const ECHO: u16 = 3111;
    const SINK: u16 = 3112;
    let (out, code) = exec(
        &backend,
        id,
        &format!(
            "ip link set lo up 2>/dev/null; \
             command -v vz-e2e-echo || echo MISSING-HELPER; \
             setsid vz-e2e-echo {ECHO} {SINK} </dev/null >/dev/null 2>&1 & \
             sleep 1"
        ),
    )
    .await;
    assert_eq!(code, Some(0), "starting loopback servers; out={out}");
    assert!(
        !out.contains("MISSING-HELPER"),
        "vz-e2e-echo not in the rootfs — stage it via make-test-rootfs.sh; out={out}"
    );

    // 1. Loopback reach: round-trip bytes through the relay to the
    //    127.0.0.1 echo server.
    let mut echo = relay_connect(&backend, id, ECHO).await;
    echo.write_all(b"ping-vz").await.expect("relay write");
    let mut got = [0u8; 7];
    echo.read_exact(&mut got).await.expect("relay read");
    assert_eq!(&got, b"ping-vz", "echo over the loopback relay");

    // 2. No head-of-line blocking: open a relay stream to the black-hole
    //    server and hold it open (it never replies). A SECOND relay stream
    //    to the echo server must still round-trip promptly — proving the
    //    stalled connection didn't monopolise the vsock device.
    let mut _sink = relay_connect(&backend, id, SINK).await;
    _sink.write_all(b"stall").await.expect("sink write");

    let round_trip = tokio::time::timeout(Duration::from_secs(5), async {
        let mut echo2 = relay_connect(&backend, id, ECHO).await;
        echo2.write_all(b"second").await.expect("relay write 2");
        let mut got2 = [0u8; 6];
        echo2.read_exact(&mut got2).await.expect("relay read 2");
        got2
    })
    .await
    .expect("second relay stream must not be HOL-blocked by the stalled one");
    assert_eq!(&round_trip, b"second", "second echo while first is stalled");

    backend.destroy(id).await.expect("destroy");
}

/// WARM restore: a snapshot and restore round-trip preserves guest
/// memory, not just disk. The tmpfs marker is the memory proof: a cold
/// boot loses it. Also pins StepClock: the resumed guest's frozen clock
/// is host-pushed back within the threshold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot: run via `just vz-e2e` (macOS + codesigned + staged artifacts)"]
async fn e2e_vz_warm_restore_preserves_memory_and_clock() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let b = backend(&env, work.path());

    let id = SandboxId::new();
    let rootfs = session_rootfs(&env, work.path(), id);
    b.create(id, spec(&rootfs)).await.expect("create");
    await_agent(&b, id).await;
    let (_, code) = exec(
        &b,
        id,
        "echo disk-marker > /root/warm.txt && echo mem-marker > /dev/shm/warm.txt",
    )
    .await;
    assert_eq!(code, Some(0), "write markers");

    let (meta, disk) = snapshot_with_disk(&b, id, &rootfs, work.path()).await;
    let machine_state = work
        .path()
        .join("sb/snapshots")
        .join(meta.id.to_string())
        .join("machine.vzs");
    if !machine_state.exists() {
        // Best-effort save failed (locked keychain on a headless host).
        // The cold path is covered by the fallback test; nothing warm
        // to assert here.
        eprintln!(
            "SKIP: machine.vzs was not saved (locked login keychain?) — warm restore \
             untestable on this host"
        );
        b.destroy(id).await.expect("destroy");
        return;
    }
    b.destroy(id).await.expect("destroy");

    // Let real time run ahead of the frozen guest clock so the
    // StepClock assertion below is meaningful (the guest steps only
    // past a 2s threshold).
    tokio::time::sleep(Duration::from_secs(4)).await;

    let id2 = SandboxId::new();
    b.restore(id2, meta, disks(&disk)).await.expect("restore");
    await_agent(&b, id2).await;
    let (out, code) = exec(&b, id2, "cat /root/warm.txt /dev/shm/warm.txt").await;
    assert_eq!(code, Some(0), "post-restore read; out={out}");
    assert!(out.contains("disk-marker"), "disk marker survives: {out}");
    assert!(
        out.contains("mem-marker"),
        "tmpfs marker must survive a WARM restore (memory proof — a cold boot would \
         lose it): {out}",
    );

    // Clock: the resumed guest was frozen for ≥4s; StepClock must have
    // pushed it back to within a few seconds of the host.
    let host_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let (out, code) = exec(&b, id2, "date +%s").await;
    assert_eq!(code, Some(0), "guest date; out={out}");
    let guest_now: i64 = out.trim().parse().expect("guest epoch seconds");
    let skew = (guest_now - host_now).abs();
    assert!(
        skew <= 3,
        "warm-restored guest clock must be host-stepped (StepClock); skew was {skew}s",
    );

    b.destroy(id2).await.expect("destroy 2");
}

/// The cold-boot FALLBACK. Deleting machine.vzs fails the warm gate;
/// the restore must cold-boot the same disk: the disk write survives,
/// the tmpfs marker is gone (fresh memory), and the session is functional.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot: run via `just vz-e2e` (macOS + codesigned + staged artifacts)"]
async fn e2e_vz_warm_restore_falls_back_to_cold() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let b = backend(&env, work.path());

    let id = SandboxId::new();
    let rootfs = session_rootfs(&env, work.path(), id);
    b.create(id, spec(&rootfs)).await.expect("create");
    await_agent(&b, id).await;
    let (_, code) = exec(
        &b,
        id,
        "echo disk-marker > /root/cold.txt && echo mem-marker > /dev/shm/cold.txt",
    )
    .await;
    assert_eq!(code, Some(0), "write markers");
    let (meta, disk) = snapshot_with_disk(&b, id, &rootfs, work.path()).await;
    b.destroy(id).await.expect("destroy");

    // Fail the warm gate: remove the machine state.
    let machine_state = work
        .path()
        .join("sb/snapshots")
        .join(meta.id.to_string())
        .join("machine.vzs");
    let _ = std::fs::remove_file(&machine_state);

    let id2 = SandboxId::new();
    b.restore(id2, meta, disks(&disk))
        .await
        .expect("restore (cold fallback)");
    await_agent(&b, id2).await;
    let (out, code) = exec(
        &b,
        id2,
        "cat /root/cold.txt; ls /dev/shm/cold.txt 2>/dev/null || echo TMPFS-GONE",
    )
    .await;
    assert_eq!(code, Some(0), "post-fallback read; out={out}");
    assert!(
        out.contains("disk-marker"),
        "disk write must survive the cold fallback: {out}",
    );
    assert!(
        out.contains("TMPFS-GONE"),
        "tmpfs must be fresh on a cold boot (memory NOT restored): {out}",
    );

    b.destroy(id2).await.expect("destroy 2");
}

/// Soft egress steering. The backend passes
/// `ENGRAM_EGRESS=<proxy>:<dns>:<metadata>` on the kernel cmdline; the
/// init shim installs DNAT rules for tcp/443, {udp,tcp}/53, and the
/// metadata address. The full traffic path through a proxy is exercised
/// by `just dev`; this harness runs no proxy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot: run via `just vz-e2e` (macOS + codesigned + staged artifacts)"]
async fn e2e_vz_egress_steering_installs_guest_redirect() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let b = VzBackend::new(
        work.path().join("sb"),
        VzConfig::with_kernel(env.kernel.clone())
            .with_bundle_dir(env.bundle_dir.clone())
            .with_egress_ports(18443, 18053, 13338),
    )
    .expect("VzBackend::new");

    let id = SandboxId::new();
    b.create(id, spec(&session_rootfs(&env, work.path(), id)))
        .await
        .expect("create");
    await_agent(&b, id).await;

    let (out, code) = exec(&b, id, "iptables -t nat -S OUTPUT").await;
    assert_eq!(code, Some(0), "iptables list; out={out}");
    assert!(
        out.contains("--dport 443") && out.contains(":18443"),
        "443 DNAT to the proxy port must be installed; rules: {out}",
    );
    assert!(
        out.contains("--dport 53") && out.contains(":18053"),
        "53 DNAT to the dns port must be installed; rules: {out}",
    );
    assert!(
        out.contains("169.254.169.254") && out.contains(":13338"),
        "metadata DNAT must target only the metadata address; rules: {out}",
    );

    b.destroy(id).await.expect("destroy");
}

/// Crash detection: a guest that stops its own VM is noticed.
/// `probe_sandbox` reports `process_alive=false` and `list()` drops the
/// sandbox; the map entry survives until `destroy`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot: run via `just vz-e2e` (macOS + codesigned + staged artifacts)"]
async fn e2e_vz_crash_detection_marks_dead() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let backend = backend(&env, work.path());

    let id = SandboxId::new();
    backend
        .create(id, spec(&session_rootfs(&env, work.path(), id)))
        .await
        .expect("create");
    await_agent(&backend, id).await;
    let probe = backend.probe_sandbox(id).await.expect("probe");
    assert!(
        probe.known_to_backend && probe.process_alive,
        "healthy VM must probe alive: {probe:?}"
    );

    // Guest-initiated stop. The exec stream dies mid-flight with the VM;
    // ignore its outcome — the assertion is what the backend REPORTS.
    let _ = tokio::time::timeout(Duration::from_secs(5), exec(&backend, id, "poweroff -f")).await;

    // The stop delegate fires on the VM's dispatch queue; poll the probe.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        let p = backend.probe_sandbox(id).await.expect("probe");
        if p.known_to_backend && !p.process_alive {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "VM never probed dead after guest poweroff: {p:?}",
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        !backend.list().await.expect("list").contains(&id),
        "dead sandbox must drop out of list() (the heartbeat's running_sandboxes)",
    );

    backend
        .destroy(id)
        .await
        .expect("destroy tolerates an already-stopped VM");
}

/// External pause and resume. Pins:
///   - pause freezes the guest (an exec makes no progress),
///   - a snapshot of a paused VM works and does not resume it,
///   - resume revives the guest and the vsock control channel answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot: run via `just vz-e2e` (macOS + codesigned + staged artifacts)"]
async fn e2e_vz_pause_freezes_and_resume_revives() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let backend = backend(&env, work.path());

    let id = SandboxId::new();
    backend
        .create(id, spec(&session_rootfs(&env, work.path(), id)))
        .await
        .expect("create");
    await_agent(&backend, id).await;
    let (out, code) = exec(&backend, id, "echo pre-pause").await;
    assert_eq!(code, Some(0), "pre-pause exec; out={out}");

    // Park. A frozen guest can't serve a fresh exec: either the vsock
    // dial hangs (timeout), submission fails, or the stream ends with NO
    // Exit event. Any Exit event proves the guest made progress.
    backend.pause(id).await.expect("pause");
    backend.pause(id).await.expect("pause is idempotent");
    let frozen = tokio::time::timeout(
        Duration::from_secs(3),
        try_exec(&backend, id, "echo should-not-run"),
    )
    .await;
    assert!(
        !frozen.as_ref().is_ok_and(exec_has_terminal_exit),
        "exec completed against a paused VM — pause didn't freeze the guest: {frozen:?}",
    );

    // Snapshot of the parked VM (rung-2→3 descent): idempotent pause,
    // flush skipped, and the VM stays parked afterwards.
    let _meta = backend.snapshot(id).await.expect("snapshot of parked VM");
    let still_frozen = tokio::time::timeout(
        Duration::from_secs(3),
        try_exec(&backend, id, "echo should-still-not-run"),
    )
    .await;
    assert!(
        !still_frozen.as_ref().is_ok_and(exec_has_terminal_exit),
        "snapshot un-parked the VM as a side effect: {still_frozen:?}",
    );

    // Un-park: the guest revives and the control channel answers.
    backend.resume(id).await.expect("resume");
    backend.resume(id).await.expect("resume is idempotent");
    let (out, code) = exec(&backend, id, "echo post-resume").await;
    assert_eq!(code, Some(0), "post-resume exec; out={out}");
    assert!(out.contains("post-resume"), "post-resume stdout: {out}");

    backend.destroy(id).await.expect("destroy");
}

/// The in-guest forge credential broker over vsock 1028. Registers a
/// `ForgeSink` that echoes the broker token back inside the minted
/// password, runs `engram-agentd forge-credential` inside the guest (the
/// helper git invokes in real sessions), and asserts the round-trip:
///
///   guest `engram-agentd forge-credential`
///     → dials AF_VSOCK host:1028
///     → VZ vsock bridge listener → our ForgeSink
///     → sink reads the `ForgeRequest`, writes a `ForgeResponse`
///     → agentd prints the credential password to stdout
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot: run via `just vz-e2e` (macOS + codesigned + staged artifacts)"]
async fn e2e_vz_forge_credential_round_trips() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let backend = backend(&env, work.path());

    // Echo the broker token back inside the password so the assertion
    // proves the in-guest env → ForgeRequest plumbing is intact
    // (mirrors the FC test's sink).
    let sink: engram_core::traits::sandbox::ForgeSink = Arc::new(move |mut stream| {
        tokio::spawn(async move {
            let req: ForgeRequest = match read_msg(&mut stream).await {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("forge sink: read failed: {e}");
                    return;
                }
            };
            let resp = match req.op {
                ForgeOp::FetchCredential { .. } => ForgeResponse::Credential {
                    username: "x-access-token".into(),
                    password: format!("ghs_canned_{}", req.broker_token),
                },
                ForgeOp::FetchOAuthCredential
                | ForgeOp::UpdateOAuthCredential { .. }
                | ForgeOp::ReportOAuthCredentialBroken { .. } => ForgeResponse::Error {
                    message: "unexpected OAuth request".into(),
                },
            };
            let _ = write_msg(&mut stream, &resp).await;
        });
    });
    backend.set_forge_sink(sink);

    let id = SandboxId::new();
    backend
        .create(id, spec(&session_rootfs(&env, work.path(), id)))
        .await
        .expect("create");
    await_agent(&backend, id).await;

    // The exec'd helper needs the session env the harness normally
    // carries: the session id, the broker token, and the transport pin.
    let mut exec_env = HashMap::new();
    exec_env.insert(
        "ENGRAM_SESSION_ID".to_string(),
        engram_core::types::ids::SessionId::new().to_string(),
    );
    exec_env.insert("ENGRAM_FORGE_TOKEN".to_string(), "tok-abc123".to_string());
    exec_env.insert("ENGRAM_TRANSPORT".to_string(), "vsock".to_string());
    let req = ExecRequest {
        command: vec![
            "/run/engram/engram-agentd".into(),
            "forge-credential".into(),
            "--host".into(),
            "github.com".into(),
        ],
        stdin: None,
        env: exec_env,
        workdir: None,
        timeout: Some(Duration::from_secs(15)),
        exec_id: None,
        stdout_offset: None,
        stderr_offset: None,
        wake: None,
    };
    let mut stream = backend.exec_stream(id, req).await.expect("exec_stream");
    let mut out = String::new();
    let mut err = String::new();
    let mut code = None;
    while let Some(ev) = stream.events.next().await {
        match ev {
            ExecEvent::Stdout(b) => out.push_str(&String::from_utf8_lossy(&b)),
            ExecEvent::Stderr(b) => err.push_str(&String::from_utf8_lossy(&b)),
            ExecEvent::Exit(c) => {
                code = c;
                break;
            }
            ExecEvent::Refused(reason) => panic!("exec refused: {reason}"),
        }
    }
    assert_eq!(code, Some(0), "forge-credential exit (stderr: {err})");
    assert_eq!(
        out, "ghs_canned_tok-abc123",
        "guest forge-credential should print the credential the sink minted \
         (stderr: {err})",
    );

    backend.destroy(id).await.expect("destroy");
}

/// Open a relay tunnel to `guest 127.0.0.1:target_port`: `open_guest_stream`
/// (vsock connect to the agentd relay) → `RelayConnect` → assert an OK
/// `RelayAck` → return the spliceable stream.
async fn relay_connect(
    backend: &VzBackend,
    id: SandboxId,
    target_port: u16,
) -> engram_core::traits::sandbox::HarnessByteStream {
    let mut stream = backend
        .open_guest_stream(id, PROXY_PORT_VSOCK_PORT)
        .await
        .expect("open_guest_stream")
        .expect("VZ open_guest_stream must return Some (real vsock)");
    write_msg(&mut stream, &RelayConnect { target_port })
        .await
        .expect("write RelayConnect");
    let ack: RelayAck = read_msg(&mut stream).await.expect("read RelayAck");
    assert!(ack.ok, "relay NAK for 127.0.0.1:{target_port}: {ack:?}");
    stream
}

/// The in-guest browser. Boots a guest with the `browser` bundle attached
/// (Xvfb, openbox, chromium, x11vnc, the launcher), calls
/// [`SandboxBackend::start_browser`], and asserts the bound VNC port.
///
/// Point `ENGRAM_VZ_SKILL_SQUASHFS` at the **browser** bundle's
/// `<bundle_dir>/<sha>.squashfs` (built by `just bundles-squashfs`). NB: the browser
/// bundle's binaries are Docker-built against glibc — this test needs a
/// glibc rootfs (a materialized session image), not the Alpine test rootfs
/// `just vz-e2e` stages.
///
/// # Scope
///
/// This asserts the **lazy-spawn → port** half of the chain — that the bundle
/// mounts on VZ and agentd brings x11vnc up and reports its port. The
/// end-to-end **RFB `RFB 003.` banner** through the relay is exercised by the
/// FC e2e (`engram-host-agent/tests/e2e_vnc.rs`, P4.1), which has the
/// host-agent `proxy_vnc` wiring this `SandboxBackend`-only harness lacks
/// (there is no `HostClient`/relay here to open a `proxy_vnc` tunnel against).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot + the Docker-built browser bundle + a glibc rootfs"]
async fn e2e_vz_browser_bundle_mounts_and_starts() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let sha = match skill_bundle_preflight(&env) {
        Some(x) => x,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let backend = backend(&env, work.path());

    let id = SandboxId::new();
    backend
        .create(
            id,
            spec_with_skill(&session_rootfs(&env, work.path(), id), &sha),
        )
        .await
        .expect("create");
    await_agent(&backend, id).await;

    // Lazy-spawn the in-guest browser stack. agentd execs `engram-browser`
    // (Xvfb → openbox → x11vnc → chromium) on first call and blocks until
    // x11vnc accepts on its port, mirroring the ttyd readiness probe.
    let start = backend
        .start_browser(id)
        .await
        .expect("start_browser spawns the browser stack and x11vnc binds");
    assert_eq!(
        start.port, 5900,
        "x11vnc default VNC port (DEFAULT_VNC_PORT); the host's proxy_vnc \
         dials this guest port",
    );

    // Idempotent re-probe: a second call must find the live stack and return
    // the same port without relaunching (the agentd spawn mutex / respawn
    // guard), the same property `start_shell` has for ttyd.
    let start2 = backend
        .start_browser(id)
        .await
        .expect("start_browser is idempotent against a live stack");
    assert_eq!(start2.port, 5900, "re-probe returns the same bound port");

    // Teardown is explicit + idempotent (the cancellable VncGrace registry
    // drives the real disconnect path; stop_browser is the belt-and-suspenders
    // pre-snapshot kill).
    backend
        .stop_browser(id)
        .await
        .expect("stop_browser tears the stack down");

    backend.destroy(id).await.expect("destroy");
}
