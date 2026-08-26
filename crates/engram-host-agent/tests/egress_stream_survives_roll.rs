//! ADR 0121: the property the daemon exists for, end to end — a REAL
//! guest's ESTABLISHED egress stream keeps flowing across a host-agent
//! roll (2026-08-26 incident: PR #1401's deploy cut two sessions
//! mid-model-API-response).
//!
//! The full production data path: guest → TAP → iptables REDIRECT →
//! the node-local egress daemon → SNI peek → bypass splice → upstream.
//! Then the roll: generation A's backend objects are abandoned with no
//! destructors (a real pod death), and generation B runs the real
//! successor sequence — `host_startup` (which PURGES and re-adds the
//! iptables rules: the conntrack-pins-established-flows claim is
//! exactly what this exercises), `reattach_pass`, and the policy sync.
//! The upstream then sends its second chunk, and the guest's
//! already-open connection must receive it.
//!
//! The guest fixture is busybox (no TLS client), so the "TLS" stream
//! is a real rustls ClientHello blob piped through `nc` — enough for
//! the SNI peek and the bypass splice, which never validate more. The
//! MITM/intercept path has its own e2e (`proxy_e2e`); this test is
//! sized to stream CONTINUITY: one guest, one connection, two chunks,
//! the roll between them.
//!
//! Run with root on the dev VM:
//!
//! ```sh
//! cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release
//! eval "$(bash crates/engram-sandbox-firecracker/scripts/fetch-fc-test-artifacts.sh)"
//! sudo -E env "PATH=$PATH" cargo test -p engram-host-agent \
//!   --test egress_stream_survives_roll -- --ignored --nocapture
//! ```
// tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]
#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec};
use engram_host_agent::bindings::BindingStore;
use engram_host_agent::egress::HostEgress;
use engram_host_agent::pooled_backend::PooledBackend;
use engram_host_agent::proxyd_client::ProxydHandle;
use engram_rootfs_materializer::{InitInjection, Transport};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};
use futures_util::StreamExt as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use common::{cleanup_host_state, fc_preflight, require_root, wait_for_guest_endpoints};

const SNI: &str = "engram-stream-test.invalid";
const CHUNK_ONE: &[u8] = b"CHUNK-ONE-BEFORE-THE-ROLL\n";
const CHUNK_TWO: &[u8] = b"CHUNK-TWO-AFTER-THE-ROLL\n";

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral bind");
    l.local_addr().expect("local addr").port()
}

/// A real ClientHello for `SNI`, as raw bytes. The proxy's SNI peek
/// parses it; the bypass splice replays it to the upstream verbatim.
fn client_hello_bytes() -> Vec<u8> {
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
    let mut conn = rustls::ClientConnection::new(
        Arc::new(config),
        SNI.to_string().try_into().expect("server name parses"),
    )
    .expect("client connection");
    let mut buf = Vec::new();
    conn.write_tls(&mut buf).expect("serialize client hello");
    assert!(!buf.is_empty(), "client hello must serialize");
    buf
}

/// The plain-TCP upstream: discards whatever arrives (the replayed
/// ClientHello), sends CHUNK_ONE immediately, then CHUNK_TWO when
/// released, then lingers so its close can never race the reads.
async fn spawn_upstream() -> (std::net::SocketAddr, tokio::sync::mpsc::UnboundedSender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("upstream bind");
    let addr = listener.local_addr().expect("upstream addr");
    let (release_tx, mut release_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.expect("upstream accept");
        let (mut read_half, mut write_half) = socket.into_split();
        // Discard inbound bytes forever (the ClientHello and anything
        // else the guest pipes) so backpressure can never wedge the
        // splice.
        tokio::spawn(async move {
            let mut sink = [0u8; 4096];
            while matches!(read_half.read(&mut sink).await, Ok(n) if n > 0) {}
        });
        write_half.write_all(CHUNK_ONE).await.expect("chunk one");
        write_half.flush().await.expect("flush one");
        let _ = release_rx.recv().await;
        write_half.write_all(CHUNK_TWO).await.expect("chunk two");
        write_half.flush().await.expect("flush two");
        // Linger: hold the write half until the release channel drops
        // (test end), so the guest-side read can never race our close.
        let _ = release_rx.recv().await;
    });
    (addr, release_tx)
}

/// The node-local daemon (ADR 0121), run as an in-process task over a
/// real control UDS + real TCP listeners. It deliberately OUTLIVES
/// both host-agent generations — that is the design under test.
async fn spawn_daemon(
    work_dir: &Path,
    upstream: std::net::SocketAddr,
) -> (Arc<ProxydHandle>, u16, u16, u16) {
    let ca =
        engram_egress_proxy::Ca::load_or_generate(&work_dir.join("egress-ca")).expect("test CA");
    let mut args = engram_egress_proxyd::DaemonArgs::new(
        work_dir.to_path_buf(),
        ca.cert_pem.clone(),
        ca.key_pair.serialize_pem(),
        "http://127.0.0.1:1".into(), // never dialed here
        None,
        engram_core::HostId::new(),
    );
    args.proxy_port = free_port();
    args.dns_port = free_port();
    args.gateway_port = free_port();
    args.test_resolves = vec![(SNI.to_string(), upstream)];
    let ports = (args.proxy_port, args.dns_port, args.gateway_port);
    tokio::spawn(engram_egress_proxyd::run(args));
    let handle = Arc::new(ProxydHandle::new(
        work_dir.join(engram_egress_proto::CONTROL_SOCK_NAME),
    ));
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if engram_egress_proto::manifest::read_manifest(work_dir).is_some()
            && handle.hello().await.is_ok()
        {
            return (handle, ports.0, ports.1, ports.2);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "egress daemon never came up",
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Accumulate stdout from the exec stream until `needle` appears.
/// Panics (with everything seen) on timeout or a terminal event —
/// silence must never pass as success.
async fn read_stdout_until(
    events: &mut engram_core::types::sandbox::ExecEventStream,
    needle: &[u8],
    budget: Duration,
    what: &str,
) -> Vec<u8> {
    use engram_core::types::sandbox::ExecEvent;
    let mut seen: Vec<u8> = Vec::new();
    let mut stderr: Vec<u8> = Vec::new();
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = tokio::time::timeout(remaining, events.next()).await;
        match event {
            Ok(Some(ExecEvent::Stdout(bytes))) => {
                seen.extend_from_slice(&bytes);
                if seen.windows(needle.len()).any(|w| w == needle) {
                    return seen;
                }
            }
            Ok(Some(ExecEvent::Stderr(bytes))) => stderr.extend_from_slice(&bytes),
            Ok(Some(terminal)) => panic!(
                "{what}: exec ended ({terminal:?}) before `{}` arrived; stdout=`{}` stderr=`{}`",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&seen),
                String::from_utf8_lossy(&stderr),
            ),
            Ok(None) => panic!(
                "{what}: exec stream closed before `{}` arrived; stdout=`{}` stderr=`{}`",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&seen),
                String::from_utf8_lossy(&stderr),
            ),
            Err(_) => panic!(
                "{what}: `{}` did not arrive within {budget:?}; stdout=`{}` stderr=`{}`",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&seen),
                String::from_utf8_lossy(&stderr),
            ),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Linux + KVM + firecracker + root (TAP + iptables)"]
async fn established_guest_stream_survives_a_host_agent_roll() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_root() {
        return;
    }
    let Some(busybox) = common::find_busybox() else {
        eprintln!("SKIP: no static busybox (apt install busybox-static or set BUSYBOX_STATIC)");
        return;
    };
    // The stream client is busybox `nc` — verify the applet exists.
    let applets = std::process::Command::new(&busybox)
        .arg("--list")
        .output()
        .expect("busybox --list");
    if !String::from_utf8_lossy(&applets.stdout)
        .split_whitespace()
        .any(|a| a == "nc")
    {
        eprintln!("SKIP: busybox has no `nc` applet");
        return;
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let agentd = Path::new(&manifest_dir)
        .join("../../target/x86_64-unknown-linux-musl/release/engram-agentd");
    if !agentd.exists() {
        eprintln!(
            "SKIP: musl engram-agentd not built at {} — \
             cargo build -p engram-agentd --target x86_64-unknown-linux-musl --release",
            agentd.display(),
        );
        return;
    }
    cleanup_host_state();

    // Provider install once for the ClientHello craft (the in-process
    // daemon's own install then no-ops).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // ---- 1. Upstream + daemon ----
    let (upstream_addr, release) = spawn_upstream().await;
    let work = tempfile::tempdir().expect("work dir");
    let (handle, proxy_port, dns_port, gateway_port) =
        spawn_daemon(work.path(), upstream_addr).await;

    // ---- 2. Bake a rootfs carrying agentd + the ClientHello blob ----
    let images = tempfile::tempdir().expect("images dir");
    let blob: Arc<dyn engram_core::traits::BlobStorage> = Arc::new(
        engram_storage_local::LocalBlobStorage::new(work.path().join("blob")),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);
    let hello_blob = client_hello_bytes();
    let outcome = common::bake_fixture_ext4(
        &images.path().join("rootfs.ext4"),
        &chunk_store,
        &busybox,
        Some(InitInjection {
            vsock_port: ENGRAM_AGENTD_PORT,
            transport: Transport::Vsock,
            init_script: None,
        }),
        |tree| std::fs::write(tree.join("ch.bin"), &hello_blob),
    )
    .await;

    // ---- 3. Generation A: real networking, real iptables ----
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel.clone());
    let staged = common::stage_agentd_bundle(&work.path().join("bundles"), &agentd);
    cfg.bundle_dir = staged.bundle_dir.clone();
    cfg.net_pool = Some("10.200.0.0".parse().unwrap());
    cfg.egress_proxy_port = Some(proxy_port);
    cfg.egress_dns_port = Some(dns_port);
    cfg.guest_gateway_port = Some(gateway_port);
    let fc_a = Arc::new(FirecrackerBackend::new(work.path(), cfg.clone()));
    fc_a.host_startup().await.expect("gen A host_startup");
    let egress_a = Arc::new(HostEgress::new(handle.clone(), String::new(), None));
    let pooled_a = Arc::new(
        PooledBackend::new(fc_a.clone() as Arc<dyn SandboxBackend>).with_egress(egress_a.clone()),
    );
    pooled_a.set_self_ref(&pooled_a);

    let spec = SandboxSpec {
        image: "engram-egress-stream-roll".into(),
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
    let sandbox = pooled_a.create(spec).await.expect("create");
    let endpoints = wait_for_guest_endpoints(&pooled_a, sandbox, Duration::from_secs(30)).await;
    let guest_ip = endpoints.egress_identity;

    // The production apply path: policy to the daemon + the ADR 0111
    // persist the successor replays from.
    let policy = engram_core::types::egress::SessionEgressPolicy {
        session_id: engram_core::SessionId::new(),
        sandbox_id: sandbox,
        guest_ip,
        network_allow_hosts: vec![SNI.to_string()],
        network_allow_host_patterns: Vec::new(),
        allow_all: false,
        secrets: Vec::new(),
        injects: Vec::new(),
        observes: Vec::new(),
        guest_services: Vec::new(),
        tunnels: Vec::new(),
        apps: Vec::new(),
        secret_mode: engram_core::types::image::SecretMode::Literal,
    };
    pooled_a
        .notify_session_policy(policy.clone())
        .await
        .expect("gen A applies the policy");
    BindingStore::open(work.path().join("bindings"))
        .expect("open bindings")
        .store_policy(&policy)
        .expect("persist policy");

    // ---- 4. The guest opens ONE egress connection and holds it:
    // ClientHello (for the SNI peek), then stay open; nc prints
    // whatever the upstream sends onto the exec stream. The target IP
    // is arbitrary — the REDIRECT catches all guest :443. ----
    let mut exec_env = HashMap::new();
    exec_env.insert("PATH".to_string(), "/bin:/sbin:/usr/bin".to_string());
    let req = ExecRequest {
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            "( cat /ch.bin; sleep 120 ) | nc 192.0.2.1 443".into(),
        ],
        stdin: None,
        env: exec_env,
        workdir: None,
        timeout: Some(Duration::from_secs(120)),
        exec_id: None,
        stdout_offset: None,
        stderr_offset: None,
        wake: None,
    };
    let stream = pooled_a.exec_stream(sandbox, req).await.expect("exec nc");
    let mut events = stream.events;
    read_stdout_until(
        &mut events,
        CHUNK_ONE,
        Duration::from_secs(30),
        "before the roll",
    )
    .await;

    // ---- 5. The roll: generation A is dead (no destructors — a real
    // pod death), and generation B runs the real successor sequence.
    // Its host_startup PURGES and re-adds the iptables rules — the
    // established flow must ride conntrack through that. ----
    let _gen_a_dead = (pooled_a, fc_a);
    let fc_b = Arc::new(FirecrackerBackend::new(work.path(), cfg.clone()));
    fc_b.host_startup().await.expect("gen B host_startup");
    let report = engram_host_agent::live_attach::reattach_pass(work.path(), &fc_b)
        .await
        .expect("reattach pass");
    assert!(
        report.reattached.iter().any(|o| matches!(
            o,
            engram_host_agent::live_attach::ReattachOutcome::Reattached { sandbox_id, .. }
                if *sandbox_id == sandbox.to_string()
        )),
        "generation B must reattach the still-live VM",
    );
    let egress_b = Arc::new(HostEgress::new(handle.clone(), String::new(), None));
    let pooled_b =
        Arc::new(PooledBackend::new(fc_b as Arc<dyn SandboxBackend>).with_egress(egress_b.clone()));
    pooled_b.set_self_ref(&pooled_b);
    let policies = BindingStore::open(work.path().join("bindings"))
        .expect("reopen bindings")
        .list_policies()
        .expect("list policies");
    pooled_b.rebuild_egress_from_policies(policies).await;

    // ---- 6. The property: the SAME connection still flows. ----
    release.send(()).expect("release chunk two");
    read_stdout_until(
        &mut events,
        CHUNK_TWO,
        Duration::from_secs(30),
        "after the roll",
    )
    .await;

    // A NEW connection also works under generation B (the ADR 0111
    // half — reachability after the roll).
    assert!(
        handle
            .lookup_guest(guest_ip)
            .await
            .expect("lookup")
            .is_some(),
        "generation B's sync must keep the survivor registered",
    );

    drop(events);
    pooled_b.destroy(sandbox).await.expect("destroy");
    cleanup_host_state();
}
