//! ADR 0066: end-to-end FC test for the vsock port relay — the fix that lets a
//! preview reach a dev server bound to the guest's **`127.0.0.1`** (Vite, Tilt),
//! which a direct dial_ip dial cannot.
//!
//! Boots a real microVM with `engram-agentd` baked in (carrying the port-relay
//! listener on `PROXY_PORT_VSOCK_PORT`), starts two concurrent loopback servers
//! INSIDE the guest via socat, then exercises the relay through
//! `SandboxBackend::open_guest_stream` + a `RelayConnect` header:
//!
//!   host `open_guest_stream(1030)` → FC vsock CONNECT → agentd port-relay
//!     → agentd dials `127.0.0.1:<port>` → raw byte splice
//!
//! Assertions:
//!   1. **loopback reach** (the regression): an echo server bound to
//!      `127.0.0.1` round-trips bytes — proving the relay reaches guest
//!      loopback, which `dial_ip:port` never could.
//!   2. **no head-of-line blocking**: with one connection backed up (its source
//!      wants to push a lot but its reader parks), the others must each still
//!      read a small chunk and finish under a tight bound.
//!
//! Small + fast on purpose: HOL-freedom is a *completion* property, so it's
//! proven with tiny reads + a tight bound — never a big transfer (that only
//! measures throughput, which is slow and inflates the FC lane; benchmark
//! throughput ad hoc on the dev VM, never in CI).
//!
//! Heavy (~40 s on the dev VM). Same preconditions as `forge_loopback`; run via
//! `scripts/run-boot-test.sh proxy_port_loopback`.

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::ids::SandboxId;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_harness_proto::{read_msg, write_msg, RelayAck, RelayConnect, PROXY_PORT_VSOCK_PORT};
use engram_image_builder::{BuildRequest, Builder, DockerCli, Format, InitInjection};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use common::{fc_preflight, require_bin};

/// Guest loopback port for the echo (correctness) server.
const ECHO_PORT: u16 = 9090;
/// Guest loopback port for the backed-up-source (HOL) server.
const SOURCE_PORT: u16 = 9091;
/// Size of the source stream socat offers per connection. Large enough that the
/// "hog" connection (which reads a little then parks) stays genuinely backed up
/// — a full credit window in flight — but nobody drains it, so it costs ~nothing.
const SOURCE_STREAM_BYTES: usize = 32 * 1024 * 1024;
/// Bytes each ACTIVE connection reads in the HOL test. Deliberately small:
/// HOL-freedom is a *completion* property, so proving it needs little data. A
/// large transfer would just measure throughput and inflate the FC lane —
/// measure throughput ad hoc on the dev VM, never in CI.
const ACTIVE_READ_BYTES: usize = 256 * 1024;
/// Tight ceiling for the active reads while one peer is backed up: they move
/// only ~2 MiB total, so seconds is plenty. A HOL regression hangs here instead.
const HOL_ACTIVE_DEADLINE: Duration = Duration::from_secs(15);
/// Concurrent forwarded connections in the HOL test (1 backed-up + N-1 active).
const HOL_CONNS: usize = 8;

#[tokio::test]
#[ignore = "requires Linux + KVM + firecracker + Docker; bakes a rootfs and boots a microVM"]
async fn port_relay_reaches_guest_loopback_without_hol_blocking() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_bin("docker") || !require_bin("mke2fs") || !require_bin("mksquashfs") {
        return;
    }

    // ---- 0. Prebuilt musl agentd (carries the ADR 0066 port relay). ----
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target_root = Path::new(&manifest).join("..").join("..").join("target");
    let agentd_bin = target_root
        .join("x86_64-unknown-linux-musl")
        .join("release")
        .join("engram-agentd");
    if !agentd_bin.exists() {
        eprintln!(
            "SKIP: missing prebuilt musl engram-agentd (expected at {}). Rebuild via:\n  \
             bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh proxy_port_loopback",
            agentd_bin.display(),
        );
        return;
    }

    // ---- 1. Bake a minimal image with socat (concurrent loopback servers). ----
    let src = tempfile::tempdir().expect("source dir");
    std::fs::write(
        src.path().join("Dockerfile"),
        // `iproute2` for `ip link set lo up` — this minimal test image's default
        // init doesn't bring up loopback, and the relay dials `127.0.0.1` (prod
        // guest images bring `lo` up; dev servers there bind loopback fine).
        "FROM debian:bookworm-slim\n\
         RUN apt-get update && apt-get install -y --no-install-recommends socat coreutils iproute2 \
         && rm -rf /var/lib/apt/lists/*\n\
         RUN mkdir -p /workspace\n",
    )
    .unwrap();
    std::fs::write(
        src.path().join("engram.toml"),
        "name = \"proxy-port-loopback-test\"\n",
    )
    .unwrap();

    let images = tempfile::tempdir().expect("images dir");
    let chunk_root = tempfile::tempdir().expect("chunk store root");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(chunk_root.path().to_path_buf()),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);
    let baker = Builder::new(DockerCli::new(), chunk_store);
    let outcome = baker
        .build(&BuildRequest {
            source: src.path().to_path_buf(),
            repo: "proxy-port-loopback-test".into(),
            tag: "warm-1".into(),
            images_dir: images.path().to_path_buf(),
            format: Format::Ext4,
            init_injection: Some(InitInjection {
                vsock_port: ENGRAM_AGENTD_PORT,
                transport: engram_image_builder::Transport::Vsock,
                init_script: None,
            }),
        })
        .await
        .expect("ext4 bake with agent injection");

    // ---- 2. FC backend + sandbox. ----
    let work = tempfile::tempdir().expect("work dir");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    // ADR 0080: agentd rides its reserved bundle slot — stage the fixture
    // bundle (agentd + stamp + sentinel) and point the backend at it.
    let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agentd_bin);
    cfg.bundle_dir = staged.bundle_dir.clone();
    cfg.net_pool = None;
    cfg.default_boot_args = "console=ttyS0 reboot=k panic=1 pci=off init=/sbin/engram-init".into();
    let backend = FirecrackerBackend::new(work.path(), cfg);

    let spec = SandboxSpec {
        image: "proxy-port-loopback-test".into(),
        rootfs_source: Some(outcome.rootfs_path),
        image_uri: None,
        rootfs_manifest: None,
        cpu: CpuLimit { vcpus: 2 },
        memory: MemoryLimit { max_mib: 512 },
        disk: DiskLimit { max_gib: 1 },
        ttl: None,
        env: HashMap::new(),
        workdir: None,
        network: Default::default(),
        aux_ro_drives: vec![staged.agentd_slot()],
    };
    let sandbox_id = backend.create(spec).await.expect("create sandbox");
    std::env::set_var("ENGRAM_FC_KEEP_JAIL_ON_FAILURE", "1");

    // ---- 3. Start two concurrent loopback servers inside the guest. ----
    // `setsid … &` detaches them from the exec's session so they survive the
    // exec returning; `fork` makes socat spawn a fresh handler per connection so
    // a stalled connection can never block another AT THE SERVER (any HOL we see
    // is then the relay's, which is what the test is guarding).
    wait_for_agent(&backend, sandbox_id, Duration::from_secs(30))
        .await
        .expect("agent never came up — see firecracker.log under work_dir");
    // Bring up loopback — the relay dials `127.0.0.1`, and this minimal image's
    // default init leaves `lo` down (a `127.0.0.1` connect otherwise fails with
    // "Network unreachable"). Prod guest images bring `lo` up already.
    exec_ok(&backend, sandbox_id, "ip link set lo up").await;
    exec_ok(
        &backend,
        sandbox_id,
        &format!(
            "setsid socat TCP-LISTEN:{ECHO_PORT},bind=127.0.0.1,fork,reuseaddr EXEC:cat \
             </dev/null >/dev/null 2>&1 &"
        ),
    )
    .await;
    exec_ok(
        &backend,
        sandbox_id,
        &format!(
            "setsid socat TCP-LISTEN:{SOURCE_PORT},bind=127.0.0.1,fork,reuseaddr \
             EXEC:'head -c {SOURCE_STREAM_BYTES} /dev/zero' </dev/null >/dev/null 2>&1 &"
        ),
    )
    .await;

    // ---- 4. Assertion 1: loopback reach (the ADR 0066 regression). ----
    // The relay's own 3 s connection-refused retry absorbs the brief window
    // before socat finishes binding.
    let mut echo = relay_connect(&backend, sandbox_id, ECHO_PORT).await;
    echo.write_all(b"ping-0066").await.expect("write echo");
    let mut got = [0u8; 9];
    echo.read_exact(&mut got).await.expect("read echo");
    assert_eq!(
        &got, b"ping-0066",
        "echo round-trip over the loopback relay"
    );
    drop(echo);

    // ---- 5. Assertion 2: no head-of-line blocking. ----
    // One "hog" connection: its source wants to push a lot, but the client reads
    // a few KiB then parks — so it stays backed up (a full credit window in
    // flight), the realistic HOL threat. The other connections must each read a
    // small chunk and finish under a TIGHT bound while the hog is parked; if a
    // backed-up connection could stall the shared virtio-vsock device, they'd
    // hang. Kept small on purpose (see ACTIVE_READ_BYTES): proving HOL-freedom is
    // a completion assertion, not a throughput benchmark.
    let mut hog = relay_connect(&backend, sandbox_id, SOURCE_PORT).await;
    let mut sink = [0u8; 4096];
    hog.read_exact(&mut sink)
        .await
        .expect("hog primes then parks");
    // …hog held live below, backed up.

    let mut active = Vec::new();
    for _ in 0..HOL_CONNS - 1 {
        active.push(relay_connect(&backend, sandbox_id, SOURCE_PORT).await);
    }
    let start = Instant::now();
    let mut handles = Vec::new();
    for mut conn in active {
        handles.push(tokio::spawn(async move {
            let mut buf = vec![0u8; ACTIVE_READ_BYTES];
            conn.read_exact(&mut buf)
                .await
                .expect("active connection must make progress while a peer is backed up");
            // drop `conn` → its source is torn down; no full drain needed.
        }));
    }
    for h in handles {
        tokio::time::timeout(HOL_ACTIVE_DEADLINE, h)
            .await
            .expect("an active connection hung behind the backed-up peer — HOL regression?")
            .expect("active connection task panicked");
    }
    eprintln!(
        "HOL: {} active connections each read {} KiB in {:?} while 1 stayed backed up",
        HOL_CONNS - 1,
        ACTIVE_READ_BYTES / 1024,
        start.elapsed(),
    );
    drop(hog);

    backend.destroy(sandbox_id).await.expect("destroy sandbox");
}

/// Open a relay tunnel to `guest 127.0.0.1:target_port`: vsock connect →
/// `RelayConnect` → assert an OK `RelayAck` → return the spliceable stream.
async fn relay_connect(
    backend: &FirecrackerBackend,
    sandbox_id: SandboxId,
    target_port: u16,
) -> engram_core::traits::sandbox::HarnessByteStream {
    let mut stream = backend
        .open_guest_stream(sandbox_id, PROXY_PORT_VSOCK_PORT)
        .await
        .expect("open_guest_stream")
        .expect("FC backend must expose a vsock relay stream");
    write_msg(&mut stream, &RelayConnect { target_port })
        .await
        .expect("write RelayConnect");
    let ack: RelayAck = read_msg(&mut stream).await.expect("read RelayAck");
    assert!(ack.ok, "relay NAK for 127.0.0.1:{target_port}: {ack:?}");
    stream
}

/// `exec` a `sh -c` one-liner in the guest and assert it exits 0.
async fn exec_ok(backend: &FirecrackerBackend, sandbox_id: SandboxId, script: &str) {
    let req = ExecRequest {
        command: vec!["/bin/sh".into(), "-c".into(), script.into()],
        stdin: None,
        env: HashMap::new(),
        workdir: None,
        timeout: Some(Duration::from_secs(10)),
    };
    let handle = backend.exec(sandbox_id, req).await.expect("exec");
    assert_eq!(
        handle.exit_status,
        Some(0),
        "guest command `{script}` failed (stderr: {})",
        String::from_utf8_lossy(&handle.stderr),
    );
}

/// Poll `exec_stream` (a trivial `true`) until the in-guest agent accepts.
async fn wait_for_agent(
    backend: &FirecrackerBackend,
    sandbox_id: SandboxId,
    budget: Duration,
) -> Result<(), engram_core::SandboxError> {
    let req = ExecRequest {
        command: vec!["/bin/true".into()],
        stdin: None,
        env: HashMap::new(),
        workdir: None,
        timeout: Some(Duration::from_secs(5)),
    };
    let deadline = Instant::now() + budget;
    let mut last_err = None;
    while Instant::now() < deadline {
        match backend.exec(sandbox_id, req.clone()).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    Err(last_err.expect("no attempts made"))
}
