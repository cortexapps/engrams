//! Live end-to-end lifecycle test for the VZ (Virtualization.framework)
//! backend. Exercises the real prod-shape path through `VzBackend` on an
//! actual booting microVM and locks in the parity fixes from ADR 0032 +
//! ADR 0096:
//!
//!   - agentd exec over the real vsock transport (ADR 0066 Phase 2),
//!   - the ADR 0080 boot contract — agentd is NOT baked into the rootfs;
//!     the stage-1 init shim execs it out of its reserved bundle slot,
//!     resolved from the staged `current.json` exactly like real sessions,
//!   - durable snapshots — a guest write with NO explicit `sync` survives a
//!     snapshot → cold-boot restore (the `flush_guest_fs` / agentd `Sync` RPC),
//!   - cold-boot restore from a clone-snapshot,
//!   - the SHELL tab — `start_shell` forwards to agentd's `StartShell` so ttyd
//!     (from the guest-tools bundle slot) is actually spawned,
//!   - the vsock port relay reaching guest loopback without head-of-line
//!     blocking (ADR 0066).
//!
//! # Gating
//!
//! macOS-only (`cfg`) and `#[ignore]` by default. **`just vz-e2e` stages
//! everything from HEAD and runs this in one command** (ADR 0096) — kernel,
//! bundles, a fresh Docker-free rootfs (Alpine minirootfs + the real init
//! shim, packed by mkext4/ADR 0093), codesigned binaries. The env contract:
//!
//!   - `ENGRAM_VZ_KERNEL_PATH` (or `~/.cache/engram-vz-test/vmlinux-arm64`,
//!     populated by `just pull-kernel`),
//!   - `ENGRAM_VZ_ROOTFS` — a bootable arm64 ext4 whose init is the ADR 0080
//!     stage-1 shim (`make-test-rootfs.sh`, or any materialized session
//!     image); agentd itself must NOT be baked in,
//!   - `ENGRAM_VZ_BUNDLE_DIR` — a staged bundle dir (`<sha>.squashfs` files +
//!     `current.json` carrying at least the `agentd` and `guest-tools`
//!     keys; `just vz-test-bundles` produces the minimal set),
//!   - `ENGRAM_VZ_REQUIRE=1` (optional) — turn every preflight SKIP into a
//!     hard failure. CI sets this so the lane can never silently regress
//!     back to skip-and-green (ADR 0096).
//!
//! Missing kernel/rootfs/bundles otherwise skip cleanly (prints `SKIP:` and
//! returns), mirroring `fc_preflight`.
//!
//! The skill/browser variants additionally need `ENGRAM_VZ_SKILL_SQUASHFS`
//! (a Docker-built bundle staged into the SAME dir as
//! `ENGRAM_VZ_BUNDLE_DIR`) and a glibc rootfs for the browser stack — they
//! stay soft-skips even under `ENGRAM_VZ_REQUIRE`.

#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{
    AuxRoDrive, CpuLimit, DiskLimit, ExecEvent, ExecRequest, MemoryLimit, SandboxSpec,
};
use engram_core::SandboxId;
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

/// ADR 0096: a failed preflight is a clean SKIP locally, a hard failure
/// under `ENGRAM_VZ_REQUIRE=1` (CI) — the exact mechanism that let the
/// live suite pass vacuously for weeks is gone.
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

fn backend(env: &VzEnv, work: &Path, with_chunks: bool) -> VzBackend {
    let b = VzBackend::new(
        work.join("sb"),
        VzConfig::with_kernel(env.kernel.clone()).with_bundle_dir(env.bundle_dir.clone()),
    )
    .expect("VzBackend::new");
    if with_chunks {
        let blob = Arc::new(engram_storage_local::LocalBlobStorage::new(
            work.join("blob"),
        ));
        b.with_chunk_store(engram_chunk_store::ChunkStore::new(blob))
    } else {
        b
    }
}

/// ADR 0080/0096: every live spec carries the symbolic agentd +
/// guest-tools reserved slots — `resolve_agentd_slot` resolves them
/// against the staged `current.json`, the same path real sessions take.
/// There is no other way to boot: the init shim panics the kernel when
/// no agentd bundle is mounted.
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
    }
}

/// ADR 0061/0096: resolve a staged skill bundle (`ENGRAM_VZ_SKILL_SQUASHFS`,
/// pointing at a `<bundle_dir>/<sha>.squashfs` produced by
/// `just bundles-squashfs`) into its sha. Always a soft skip when
/// unset/missing — the skill/browser bundles are Docker-built, which the
/// CI runner can't produce.
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
            ExecEvent::Refused(reason) => panic!("exec refused: {reason}"),
        }
    }
    (out, code)
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

/// ADR 0112 VZ parity: a spec with `swap_mib` boots with exactly one
/// writable non-vda virtio disk of that size, agentd arms it at boot
/// (mkswap + swapon + vm.swappiness), and destroy removes the backing
/// file. Mirrors the FC `swap_disk` assertions on the VZ attach path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot: run via `just vz-e2e` (macOS + codesigned + staged artifacts)"]
async fn e2e_vz_swap_drive_arms() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let backend = backend(&env, work.path(), false);

    let mut s = spec(&env.rootfs);
    s.swap_mib = Some(64);
    let id = backend.create(s).await.expect("create with swap");
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

    // Destroy removes the per-sandbox backing file (the backend's
    // work_dir is `<work>/sb` — see `backend()` above). Unlike FC's
    // unlinked-after-attach backing this file keeps its name for the
    // VM's lifetime, so its mode must be 0600 regardless of umask —
    // it holds guest memory in plaintext.
    let swap_backing = work.path().join("sb").join(format!("{id}.swap.img"));
    assert!(swap_backing.exists(), "backing exists while running");
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&swap_backing)
            .expect("backing metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "swap backing must be private");
    }
    backend.destroy(id).await.expect("destroy");
    assert!(!swap_backing.exists(), "backing removed at destroy");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot: run via `just vz-e2e` (macOS + codesigned + staged artifacts)"]
async fn e2e_vz_lifecycle() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let backend = backend(&env, work.path(), true);

    // 1. Boot + exec over the real vsock transport (ADR 0066 Phase 2; ADR
    //    0032 #3: exec must answer promptly, not 90s later — the ready-port
    //    drain listener keeps the guest handshake from stalling). The boot
    //    itself proves the ADR 0080 contract: agentd came out of its
    //    bundle slot, resolved from current.json.
    let id = backend.create(spec(&env.rootfs)).await.expect("create");
    await_agent(&backend, id).await;
    let (out, code) = exec(&backend, id, "echo hello-vz && uname -m").await;
    assert_eq!(code, Some(0), "exec exit; out={out}");
    assert!(out.contains("hello-vz"), "exec stdout: {out}");

    // 2. SHELL tab (ADR 0032 #5): start_shell must spawn ttyd (from the
    //    guest-tools bundle slot — the rootfs bakes no ttyd) and return
    //    its port, not the trait-default-7681-without-a-listener.
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
    let backend = backend(&env, work.path(), true);

    let id = backend
        .create(spec_with_skill(&env.rootfs, &sha))
        .await
        .expect("create");
    await_agent(&backend, id).await;

    // Attach order is slot-ascending (ADR 0062), so the skill (slot 0)
    // is the FIRST aux drive: /dev/vdb (after the /dev/vda rootfs).
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

/// ADR 0066 Phase 2: the port relay reaches a dev server bound to the
/// guest's `127.0.0.1` (which a direct dial_ip/eth0 dial can't), and a
/// persistent forwarded connection does NOT head-of-line block a fresh
/// one — the property real virtio-vsock gives us that the retired
/// single-stream-per-port console bridge couldn't.
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
    let backend = backend(&env, work.path(), false);

    let id = backend.create(spec(&env.rootfs)).await.expect("create");
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

    // 1. Loopback reach (the ADR 0066 regression): round-trip bytes through
    //    the relay to the 127.0.0.1 echo server.
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

/// ADR 0096 D7: WARM restore — a snapshot→restore round-trip preserves
/// guest MEMORY, not just disk. The tmpfs marker (/dev/shm — never
/// touches the virtio-blk disk) is the memory proof: a cold boot loses
/// it, only a machine-state resume carries it. Also pins the StepClock
/// path: the resumed guest's frozen CLOCK_REALTIME is host-pushed back
/// within the threshold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot: run via `just vz-e2e` (macOS + codesigned + staged artifacts)"]
async fn e2e_vz_warm_restore_preserves_memory_and_clock() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let b = backend(&env, work.path(), true);

    let id = b.create(spec(&env.rootfs)).await.expect("create");
    await_agent(&b, id).await;
    let (_, code) = exec(
        &b,
        id,
        "echo disk-marker > /root/warm.txt && echo mem-marker > /dev/shm/warm.txt",
    )
    .await;
    assert_eq!(code, Some(0), "write markers");

    let meta = b.snapshot(id).await.expect("snapshot");
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

    let id2 = b.restore(meta).await.expect("restore");
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

/// ADR 0096 D7: the cold-boot FALLBACK. Deleting machine.vzs fails the
/// warm gate; the restore must cold-boot exactly like pre-D7 — the
/// un-synced disk write survives (the pre-clone flush property), the
/// tmpfs marker is gone (fresh memory), and the session is functional.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot: run via `just vz-e2e` (macOS + codesigned + staged artifacts)"]
async fn e2e_vz_warm_restore_falls_back_to_cold() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let b = backend(&env, work.path(), true);

    let id = b.create(spec(&env.rootfs)).await.expect("create");
    await_agent(&b, id).await;
    // NO explicit sync — the disk marker's survival across the COLD
    // path is the flush_guest_fs property this test now carries
    // (lifecycle's restore leg went warm).
    let (_, code) = exec(
        &b,
        id,
        "echo disk-marker > /root/cold.txt && echo mem-marker > /dev/shm/cold.txt",
    )
    .await;
    assert_eq!(code, Some(0), "write markers");
    let meta = b.snapshot(id).await.expect("snapshot");
    b.destroy(id).await.expect("destroy");

    // Fail the warm gate: remove the machine state.
    let machine_state = work
        .path()
        .join("sb/snapshots")
        .join(meta.id.to_string())
        .join("machine.vzs");
    let _ = std::fs::remove_file(&machine_state);

    let id2 = b.restore(meta).await.expect("restore (cold fallback)");
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
        "un-synced disk write must survive the cold fallback (flush property): {out}",
    );
    assert!(
        out.contains("TMPFS-GONE"),
        "tmpfs must be fresh on a cold boot (memory NOT restored): {out}",
    );

    b.destroy(id2).await.expect("destroy 2");
}

/// ADR 0096 D6: soft egress steering. The backend passes
/// `ENGRAM_EGRESS=<proxy>:<dns>:<metadata>` on the kernel cmdline; the init shim
/// derives the NAT gateway from the guest's default route and installs
/// DNAT rules for tcp/443, {udp,tcp}/53, and the metadata address.
/// This pins the cmdline → shim → netfilter chain (the test rootfs
/// carries Alpine's iptables; images without it warn + stay open). The
/// full traffic path (proxy SNI dial, CA, allow_hosts) is exercised by
/// `just dev` — the backend-level harness runs no proxy.
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

    let id = b.create(spec(&env.rootfs)).await.expect("create");
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

/// ADR 0096 (ADR 0009 §4, VZ edition): crash detection. A guest that
/// stops its VM out from under the host-agent must be noticed eagerly:
/// the `VZVirtualMachineDelegate` shim flips the dead flag,
/// `probe_sandbox` reports `process_alive=false` from the flag + a live
/// `state()` read, and `list()` drops the sandbox so the heartbeat's
/// `running_sandboxes` reflects ground truth (the coordinator's 3-strike
/// divergence flip then fires unmodified). Cleanup stays coordinator-
/// driven — the map entry survives until `destroy`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot: run via `just vz-e2e` (macOS + codesigned + staged artifacts)"]
async fn e2e_vz_crash_detection_marks_dead() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let backend = backend(&env, work.path(), false);

    let id = backend.create(spec(&env.rootfs)).await.expect("create");
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

/// ADR 0096: external pause/resume — the coordinator's rung-2 park.
/// Until ADR 0096 `VzBackend` inherited the trait-default no-ops, so a
/// park "succeeded" while the guest kept running. Pins:
///   - pause actually freezes the guest (an exec makes no progress),
///   - snapshot of a PARKED VM works (the rung-2→3 descent: idempotent
///     pause + flush skipped) and does NOT un-park as a side effect,
///   - resume revives the guest and the vsock control channel still
///     answers (the ADR 0074 lesson from FC: never assume
///     vsock-survives-resume — pin it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot: run via `just vz-e2e` (macOS + codesigned + staged artifacts)"]
async fn e2e_vz_pause_freezes_and_resume_revives() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let backend = backend(&env, work.path(), true);

    let id = backend.create(spec(&env.rootfs)).await.expect("create");
    await_agent(&backend, id).await;
    let (out, code) = exec(&backend, id, "echo pre-pause").await;
    assert_eq!(code, Some(0), "pre-pause exec; out={out}");

    // Park. A frozen guest can't serve a fresh exec: either the vsock
    // dial hangs (timeout) or it fails fast and the stream ends with NO
    // Exit event (`code=None`). Only a completed `Some(0)` exec proves
    // the guest is still running.
    backend.pause(id).await.expect("pause");
    backend.pause(id).await.expect("pause is idempotent");
    let frozen = tokio::time::timeout(
        Duration::from_secs(3),
        exec(&backend, id, "echo should-not-run"),
    )
    .await;
    assert!(
        !matches!(&frozen, Ok((_, Some(0)))),
        "exec completed against a paused VM — pause didn't freeze the guest: {frozen:?}",
    );

    // Snapshot of the parked VM (rung-2→3 descent): idempotent pause,
    // flush skipped, and the VM stays parked afterwards.
    let meta = backend.snapshot(id).await.expect("snapshot of parked VM");
    assert!(meta.disk_manifest.is_some(), "parked snapshot still chunks");
    let still_frozen = tokio::time::timeout(
        Duration::from_secs(3),
        exec(&backend, id, "echo should-still-not-run"),
    )
    .await;
    assert!(
        !matches!(&still_frozen, Ok((_, Some(0)))),
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

/// ADR 0023/0096: the in-guest forge credential broker over vsock 1028 —
/// the VZ mirror of FC's `forge_loopback.rs`. Registers a `ForgeSink`
/// that echoes the broker token back inside the minted password, runs
/// `engram-agentd forge-credential` inside the guest (the exact helper
/// git invokes in real sessions), and asserts the round-trip:
///
///   guest `engram-agentd forge-credential`
///     → dials AF_VSOCK host:1028
///     → VZ vsock bridge listener → our ForgeSink
///     → sink reads the `ForgeRequest`, writes a `ForgeResponse`
///     → agentd prints the credential password to stdout
///
/// Until ADR 0096 the bridge had no 1028 listener and `set_forge_sink`
/// was the trait-default no-op — this dial died on connection-refused,
/// silently breaking git-credential brokering on every VZ session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "live VZ boot: run via `just vz-e2e` (macOS + codesigned + staged artifacts)"]
async fn e2e_vz_forge_credential_round_trips() {
    let env = match vz_preflight() {
        Some(e) => e,
        None => return,
    };
    let work = tempfile::tempdir().expect("workdir");
    let backend = backend(&env, work.path(), false);

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

    let id = backend.create(spec(&env.rootfs)).await.expect("create");
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

/// ADR 0065 (P4.2): VZ parity for the in-guest browser. Boots a VZ guest with
/// the **`browser`** bundle attached (`Xvfb` + `openbox` + `chromium` +
/// `x11vnc` + the `engram-browser` launcher), then calls
/// [`SandboxBackend::start_browser`] and asserts the bound VNC port — the same
/// lazy-spawn path the host's `proxy_vnc` drives before dialing the guest's
/// raw-TCP `:5900`. This mirrors [`e2e_vz_skill_bundle_attaches`] (the ADR 0061
/// bundle harness) but with the browser bundle and the `start_browser` lazy
/// spawn instead of a manual mount-and-read.
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
    let backend = backend(&env, work.path(), true);

    let id = backend
        .create(spec_with_skill(&env.rootfs, &sha))
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
