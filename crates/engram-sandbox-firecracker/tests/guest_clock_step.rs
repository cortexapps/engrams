//! The host asserts the guest's wall clock at `start_agent`.
//!
//! ## What broke, and why this test exists
//!
//! FC freezes `CLOCK_REALTIME` at snapshot capture, so every sandbox
//! restored from a base snapshot wakes up as far behind real time as its
//! snapshot is old. For a long time the only correction was guest-side:
//! agentd reads the KVM PTP device (`/dev/ptp0`) and steps itself
//! (`engram_agentd::clock`). The host sent nothing and assumed it worked.
//!
//! On 2026-08-07 it stopped working and nothing said so. The prod fleet
//! moved to a new node pool; guests restored from the 17-day-old
//! `demo:latest` base snapshot could not read a PHC on those hosts, so
//! agentd's sync became a permanent no-op and every such guest ran ~17
//! days in the past. The symptom surfaced twenty seconds later and three
//! layers away, as the agent reporting
//! `API Error: Unable to connect to API: SSL certificate is not yet valid`
//! — a guest in the past rejects any upstream certificate issued since.
//! Every review session failed for twelve hours. 19/19 sessions on that
//! image; 0/13 on images whose snapshots were hours old.
//!
//! `FirecrackerBackend::step_guest_clock` closes it by pushing the host's
//! own `CLOCK_REALTIME` (`WireRequest::StepClock`) before the harness is
//! spawned, so a correct clock no longer depends on a guest-side device
//! probe the host cannot see fail.
//!
//! ## Why the guest boots with `initcall_blacklist=ptp_kvm_init`
//!
//! Without that, this test cannot fail. agentd re-syncs from the PHC on a
//! 10 s tick AND before every exec, so on a runner with a working PTP
//! device the guest clock is correct no matter what the host does — the
//! very confound that let the host-side gap sit undetected. Blacklisting
//! the `ptp_kvm` initcall removes `/dev/ptp0`, which reproduces the prod
//! fleet's condition exactly: the host's push becomes the ONLY thing that
//! can correct the clock. Delete the `step_guest_clock` call and this
//! test fails.
//!
//! The absence of `/dev/ptp0` is asserted, not assumed — a renamed
//! initcall would otherwise silently restore the confound and leave this
//! passing for the wrong reason.
//!
//! Heavy test (bake + microVM boot, ~30 s on the dev VM), `#[ignore]`d
//! like its FC siblings and wired into ci.yml's `test-firecracker` job.
//! Preconditions match `baked_harness_loopback.rs`: Linux + KVM +
//! firecracker + prebuilt musl `engram-agentd` / `engram-harness-noop`.

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::ids::SessionId;
use engram_core::types::sandbox::{
    AgentSpec, CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec,
};
use engram_rootfs_materializer::{InitInjection, Transport};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};

use common::{drain, fc_preflight, require_bin};

/// How far into the past we shove the guest clock before `start_agent`.
/// Roughly the age of the base snapshot that stranded prod, and far
/// enough past the 2 s step threshold to be unambiguous.
const SKEW: Duration = Duration::from_secs(17 * 24 * 60 * 60);

/// How close to the host's clock the guest must land. Generous: this
/// asserts "the clock was corrected", not "the correction was precise".
/// The round trip plus a slow CI runner is worth seconds, not minutes.
const TOLERANCE_SECS: i64 = 120;

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker; bakes a rootfs and boots a microVM"]
async fn start_agent_corrects_a_guest_clock_with_no_ptp_device() {
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
    let musl_release = target_root
        .join("x86_64-unknown-linux-musl")
        .join("release");
    let agentd_bin = musl_release.join("engram-agentd");
    let noop_bin = musl_release.join("engram-harness-noop");
    if !agentd_bin.exists() || !noop_bin.exists() {
        eprintln!(
            "SKIP: missing prebuilt musl binaries. The script rebuilds them:\n  \
             bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh guest_clock_step\n  \
             Expected paths:\n    {}\n    {}",
            agentd_bin.display(),
            noop_bin.display(),
        );
        return;
    }

    // ---- 1. Bake a rootfs carrying the noop harness --------------
    let images = tempfile::tempdir().expect("images dir");
    let chunk_root = tempfile::tempdir().expect("chunk store root");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
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
        |tree| {
            use std::os::unix::fs::PermissionsExt;
            let dst_dir = tree.join("opt/noop");
            std::fs::create_dir_all(&dst_dir)?;
            let dst = dst_dir.join("harness");
            std::fs::copy(&noop_bin, &dst)?;
            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        },
    )
    .await;

    // ---- 2. Boot with ptp_kvm's initcall blacklisted -------------
    let work = tempfile::tempdir().expect("work dir");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agentd_bin);
    cfg.bundle_dir = staged.bundle_dir.clone();
    cfg.net_pool = None;
    // The one line this test turns on: no PHC in the guest, so agentd's
    // self-drive cannot mask a missing host push. See the module docs.
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off \
                             initcall_blacklist=ptp_kvm_init init=/sbin/engram-init"
        .into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    // The noop harness dials back on vsock 1026; give it somewhere to
    // land so `start_agent`'s spawn isn't racing a closed port.
    let (sink_tx, _sink_rx) =
        tokio::sync::mpsc::unbounded_channel::<engram_core::traits::HarnessByteStream>();
    let sink: engram_core::traits::HarnessSink = Arc::new(move |stream| {
        let _ = sink_tx.send(stream);
    });
    backend.set_harness_sink(sink);

    let spec = SandboxSpec {
        image: "guest-clock-step-test".into(),
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
        swap_mib: None,
    };
    let sandbox_id = backend.create(spec).await.expect("create sandbox");
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    wait_for_agent_ready(&backend, sandbox_id, Duration::from_secs(20))
        .await
        .expect("agent never came up — see firecracker.log under work_dir");

    // ---- 3. Precondition: the guest really has no PHC ------------
    // Asserted, not assumed: if a kernel change renames the initcall,
    // `/dev/ptp0` comes back, agentd self-syncs, and the rest of this
    // test would pass no matter what the host does.
    let probe = sh(
        &backend,
        sandbox_id,
        "[ -e /dev/ptp0 ] && echo yes || echo no",
    )
    .await;
    assert_eq!(
        probe.trim(),
        "no",
        "guest still has /dev/ptp0 — `initcall_blacklist=ptp_kvm_init` no longer disables the \
         KVM PTP device, so agentd's self-sync would mask the host push this test exists to \
         pin. Find the current initcall name and update the boot args.",
    );

    // ---- 4. Shove the guest clock into the past ------------------
    // Set and read back in ONE exec: agentd runs its (here inert) sync
    // before each exec, so a separate read would be a separate chance to
    // be corrected by something other than what we're testing.
    let host_before = unix_now();
    let skewed_target = host_before - SKEW.as_secs() as i64;
    let skewed = sh(
        &backend,
        sandbox_id,
        &format!("date -u -s @{skewed_target} >/dev/null 2>&1; date -u +%s"),
    )
    .await;
    let skewed: i64 = skewed.trim().parse().expect("guest clock as unix seconds");
    assert!(
        (skewed - skewed_target).abs() < 60,
        "failed to skew the guest clock: wanted ~{skewed_target}, guest reports {skewed}",
    );

    // ---- 5. start_agent must put it back ------------------------
    let session_id = SessionId::new();
    let agent = AgentSpec {
        binding_epoch: 1,
        argv: vec![
            "/opt/noop/harness".into(),
            "--port".into(),
            engram_harness_proto::HARNESS_VSOCK_PORT.to_string(),
            "--session-id".into(),
            session_id.to_string(),
        ],
        env: HashMap::new(),
        session_env: HashMap::new(),
        host_ca_pem: None,
    };
    backend
        .start_agent(sandbox_id, agent)
        .await
        .expect("start_agent");

    let after = sh(&backend, sandbox_id, "date -u +%s").await;
    let after: i64 = after.trim().parse().expect("guest clock as unix seconds");
    let drift = (after - unix_now()).abs();
    assert!(
        drift <= TOLERANCE_SECS,
        "guest clock still {drift}s from the host after start_agent (was skewed to {skewed}, \
         now {after}). With no PHC in this guest, the host's StepClock push is the only thing \
         that can correct it — check `FirecrackerBackend::step_guest_clock`.",
    );

    backend.destroy(sandbox_id).await.expect("destroy sandbox");
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("host clock after 1970")
        .as_secs() as i64
}

/// Run `script` through the guest's shell and return its stdout.
async fn sh(
    backend: &FirecrackerBackend,
    sandbox_id: engram_core::SandboxId,
    script: &str,
) -> String {
    let stream = backend
        .exec_stream(
            sandbox_id,
            ExecRequest {
                command: vec!["/bin/sh".into(), "-c".into(), script.into()],
                stdin: None,
                env: HashMap::new(),
                workdir: None,
                timeout: Some(Duration::from_secs(10)),
                exec_id: None,
                stdout_offset: None,
                stderr_offset: None,
                wake: None,
            },
        )
        .await
        .unwrap_or_else(|e| panic!("exec_stream `{script}`: {e:?}"));
    let (stdout, stderr, exit) = drain(stream.events).await;
    assert_eq!(
        exit,
        Some(0),
        "`{script}` exited {exit:?} (stderr: {})",
        String::from_utf8_lossy(&stderr),
    );
    String::from_utf8_lossy(&stdout).into_owned()
}

/// Poll `exec_stream` until agentd accepts, mirroring
/// `baked_harness_loopback.rs`'s wait: `start_agent` would block on
/// `wait_agent_ready` anyway, but we need the guest reachable BEFORE
/// then so we can skew its clock.
async fn wait_for_agent_ready(
    backend: &FirecrackerBackend,
    sandbox_id: engram_core::SandboxId,
    budget: Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + budget;
    let req = ExecRequest {
        command: vec!["true".into()],
        stdin: None,
        env: HashMap::new(),
        workdir: None,
        timeout: Some(Duration::from_secs(5)),
        exec_id: None,
        stdout_offset: None,
        stderr_offset: None,
        wake: None,
    };
    let mut last = String::new();
    while tokio::time::Instant::now() < deadline {
        match backend.exec_stream(sandbox_id, req.clone()).await {
            Ok(stream) => {
                let _ = drain(stream.events).await;
                return Ok(());
            }
            Err(e) => last = format!("{e:?}"),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(format!("agent not ready within {budget:?}: {last}"))
}
