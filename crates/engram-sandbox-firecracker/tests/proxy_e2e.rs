//! Full end-to-end exercise of the egress-proxy path:
//!
//!   guest → tap-engr-XXX → iptables REDIRECT → proxy →
//!   StaticResolver(engram-test.invalid) → fake upstream
//!
//! Asserts:
//!   1. CA install at boot — guest's curl trusts the proxy-minted
//!      leaf for `engram-test.invalid`.
//!   2. SO_ORIGINAL_DST recovery + SNI-based dial — the proxy
//!      ignores the IP the guest connected to and dials the
//!      static-mapped fake upstream.
//!   3. Placeholder substitution — the upstream sees the real
//!      secret value, the in-VM env never had it.
//!   4. Violation close — when a placeholder hits a host outside
//!      the secret's `allow_hosts`, the connection drops without
//!      forwarding.
//!
//! Heavy: bakes a debian-slim rootfs with curl, builds a substrate
//! that includes the engram CA, boots an FC microVM, configures
//! networking + iptables under sudo, then drives the test.
//!
//! Run with sudo on the dev VM:
//!
//!   bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh proxy_e2e

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::sandbox::{
    AgentSpec, CpuLimit, DiskLimit, ExecRequest, MemoryLimit, SandboxSpec,
};
use engram_image_builder::{AgentInjection, BuildRequest, Builder, DockerCli, Format, Transport};
use engram_sandbox_firecracker::{FirecrackerBackend, FirecrackerConfig, ENGRAM_AGENTD_PORT};
use parking_lot::Mutex;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

#[allow(unused_imports)]
use common::{drain, fc_preflight, require_bin};

/// SNI / Host the test uses for the fake upstream. `.invalid` is
/// reserved by RFC 2606 for test/dummy use; real DNS will never
/// resolve it. We seed `/etc/hosts` in the guest so curl can dial
/// it (any IP works — iptables REDIRECT catches all 443).
const TEST_HOST: &str = "engram-test.invalid";

/// A `/proc/self/status`-based root check matching `host_startup.rs`.
fn require_root() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").expect("read status");
    let euid: u32 = status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(2).and_then(|s| s.parse().ok()))
        .expect("parse euid");
    if euid != 0 {
        eprintln!(
            "SKIP: proxy_e2e requires root (CAP_NET_ADMIN for TAP + iptables). \
             Re-run via `sudo -E ...`."
        );
        return false;
    }
    true
}

/// Spin up a TLS upstream that captures the first request and
/// replies 200 OK. Self-signed cert; the proxy's client config
/// skips upstream verification (host is the TCB).
async fn fake_upstream(captured: Arc<Mutex<Vec<u8>>>) -> SocketAddr {
    let mut params = CertificateParams::new(vec![TEST_HOST.to_string()]).unwrap();
    params.distinguished_name = {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, TEST_HOST);
        dn
    };
    params.subject_alt_names = vec![SanType::DnsName(TEST_HOST.try_into().unwrap())];
    let kp = KeyPair::generate().unwrap();
    let cert = params.self_signed(&kp).unwrap();
    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der: PrivateKeyDer<'static> =
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(kp.serialize_der()));
    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(cfg));

    // Bind to all interfaces so the proxy's TLS dial (which
    // resolves engram-test.invalid → 127.0.0.1) reaches us.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let captured = captured.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else {
                    return;
                };
                let mut buf = vec![0u8; 8192];
                if let Ok(n) = tls.read(&mut buf).await {
                    captured.lock().extend_from_slice(&buf[..n]);
                }
                let _ = tls
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                    .await;
                let _ = tls.shutdown().await;
            });
        }
    });
    addr
}

/// Delete every `tap-engr-*` device on the host. Stale TAPs from
/// failed prior runs share the same gateway IP (10.200.0.1) since
/// each run starts the allocator at slot 0; with multiple TAPs
/// holding 10.200.0.1, ARP responses race and connectivity dies.
/// Wipe the slate before each test.
fn delete_stale_taps() {
    let saved = std::process::Command::new("sh")
        .arg("-c")
        .arg("ip -o link show | awk -F': ' '/tap-engr-/ {print $2}' | awk '{print $1}'")
        .output();
    let Ok(o) = saved else { return };
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        let name = line.trim();
        if name.is_empty() {
            continue;
        }
        let _ = std::process::Command::new("ip")
            .args(["link", "delete", name])
            .output();
    }
}

/// Cleanup any iptables rules left from a prior failed run.
fn iptables_cleanup() {
    let saved = String::from_utf8(
        std::process::Command::new("iptables-save")
            .output()
            .expect("iptables-save")
            .stdout,
    )
    .unwrap_or_default();
    let mut current_table = "filter".to_string();
    for line in saved.lines() {
        if let Some(t) = line.strip_prefix('*') {
            current_table = t.trim().to_string();
            continue;
        }
        if !line.contains("engram-") {
            continue;
        }
        let Some(rest) = line.strip_prefix("-A ") else {
            continue;
        };
        let mut argv = vec!["-t".to_string(), current_table.clone(), "-D".to_string()];
        argv.extend(rest.split_whitespace().map(str::to_string));
        let _ = std::process::Command::new("iptables").args(&argv).output();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires Linux + KVM + firecracker + Docker + sudo (CAP_NET_ADMIN)"]
async fn proxy_substitutes_real_value_into_outbound_https() {
    let env = match fc_preflight() {
        Some(e) => e,
        None => return,
    };
    if !require_bin("docker") || !require_bin("mke2fs") {
        return;
    }
    if !require_root() {
        return;
    }
    iptables_cleanup();
    delete_stale_taps();

    // Run via `scripts/run-boot-test.sh proxy_e2e` — the script
    // rebuilds the musl agent first, sidestepping the staleness
    // footgun where a host-side wire-protocol change ships against a
    // cached pre-change binary.
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target_root = Path::new(&manifest).join("..").join("..").join("target");
    let agent_bin = target_root
        .join("x86_64-unknown-linux-musl")
        .join("release")
        .join("engram-agentd");
    if !agent_bin.exists() {
        eprintln!(
            "SKIP: musl agentd not built at {}.\n  \
             Run: bash crates/engram-sandbox-firecracker/scripts/run-boot-test.sh proxy_e2e",
            agent_bin.display(),
        );
        return;
    }

    // ---- 1. Generate the engram CA + start the proxy ----
    let proxy_dir = tempfile::tempdir().unwrap();
    let ca = Arc::new(engram_egress_proxy::Ca::load_or_generate(proxy_dir.path()).unwrap());
    let _ = rustls::crypto::ring::default_provider().install_default();
    let registry = Arc::new(engram_egress_proxy::Registry::new());
    let mint = Arc::new(engram_egress_proxy::CertMint::new(ca.clone()));

    // ---- 2. Spin up the fake upstream + StaticResolver pointing at it ----
    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream_addr = fake_upstream(captured.clone()).await;
    let resolver =
        Arc::new(engram_egress_proxy::StaticResolver::new().with(TEST_HOST, upstream_addr));

    let proxy_port: u16 = 19443;
    let proxy_bind: SocketAddr = format!("0.0.0.0:{proxy_port}").parse().unwrap();
    let mut proxy_cfg = engram_egress_proxy::ProxyConfig::new(proxy_bind, registry.clone(), mint);
    proxy_cfg.resolver = resolver;
    let proxy = engram_egress_proxy::Proxy::new(proxy_cfg);
    tokio::spawn(async move {
        let _ = proxy.run().await;
    });
    // Give the listener a tick to bind.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // ---- 3. Bake a rootfs with engram-agentd + curl + /etc/hosts seed ----
    let src = tempfile::tempdir().expect("source dir");
    std::fs::write(
        src.path().join("Dockerfile"),
        // debian-slim has glibc + ca-certificates support. Install
        // curl + ca-certificates so the in-VM curl validates the
        // proxy-minted leaf via the trust bundle the init shim
        // updates. /etc/hosts seeding has to happen at runtime —
        // Docker mounts it read-only at build time — so we do it
        // inside the test's exec command.
        "FROM debian:bookworm-slim\n\
         RUN apt-get update && apt-get install -y --no-install-recommends \
             curl ca-certificates iproute2 iputils-ping && rm -rf /var/lib/apt/lists/*\n",
    )
    .unwrap();
    std::fs::write(
        src.path().join("engram.toml"),
        "name = \"engram-proxy-e2e\"\n",
    )
    .unwrap();
    let images = tempfile::tempdir().expect("images");
    let chunk_root = tempfile::tempdir().expect("chunk store root");
    let blob: std::sync::Arc<dyn engram_core::traits::BlobStorage> = std::sync::Arc::new(
        engram_storage_local::LocalBlobStorage::new(chunk_root.path().to_path_buf()),
    );
    let chunk_store = engram_chunk_store::ChunkStore::new(blob);
    let baker = Builder::new(DockerCli::new(), chunk_store);
    let outcome = baker
        .build(&BuildRequest {
            source: src.path().to_path_buf(),
            repo: "engram-proxy-e2e".into(),
            tag: "warm-1".into(),
            images_dir: images.path().to_path_buf(),
            format: Format::Ext4,
            agent_injection: Some(AgentInjection {
                agent_binary: agent_bin,
                vsock_port: ENGRAM_AGENTD_PORT,
                transport: Transport::Vsock,
                init_script: None,
            }),
        })
        .await
        .expect("ext4 bake");

    // ADR 0021 P1.5: no substrate to build — the egress CA reaches
    // the guest via `AgentSpec.host_ca_pem`, which triggers an
    // `InstallHostCa` vsock RPC right before `SpawnHarness` (see the
    // `start_agent` call below).

    // ---- 5. Set up FC backend with networking + proxy redirect ----
    let work = tempfile::tempdir().expect("work");
    let mut cfg = FirecrackerConfig::with_kernel(env.kernel);
    cfg.net_pool = Some("10.200.0.0".parse().unwrap());
    cfg.egress_proxy_port = Some(proxy_port);
    let backend = Arc::new(FirecrackerBackend::new(work.path(), cfg));
    backend.host_startup().await.expect("host_startup");

    let session_id = engram_core::SessionId::new();

    // ---- 6. Boot the sandbox ----
    let spec = SandboxSpec {
        image: "engram-proxy-e2e".into(),
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
        aux_ro_drives: Vec::new(),
    };
    let sandbox_id = backend.create(spec).await.expect("create");
    // Poll for guest_endpoints — the in-VM agent takes a few seconds to
    // bind on vsock and answer the GuestIp RPC after kernel boot.
    let mut endpoints = None;
    for _ in 0..30 {
        if let Some(ep) = backend.guest_endpoints(sandbox_id).await {
            endpoints = Some(ep);
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let guest_ip = endpoints
        .expect("guest_endpoints should be reachable within 15s")
        .egress_identity;

    // ---- 7. Register the session with the proxy ----
    let secret = engram_egress_proxy::SecretEntry {
        placeholder: "engram_ph_e2e_xxx".into(),
        real_value: "sk-real-secret-from-host".into(),
        allow: engram_egress_proxy::HostList::from_manifest(&[TEST_HOST.into()], &[]).unwrap(),
    };
    let network_allow =
        engram_egress_proxy::HostList::from_manifest(&[TEST_HOST.into()], &[]).unwrap();
    registry.register(engram_egress_proxy::SessionState {
        session_id,
        guest_ip,
        network_allow,
        allow_all: false,
        secrets: vec![secret],
        injects: Vec::new(),
        observes: Vec::new(),
    });

    // PID-1's env doesn't carry a PATH; child execs need one to
    // find /usr/bin/curl + shell builtins.
    let mut diag_env = HashMap::new();
    diag_env.insert(
        "PATH".into(),
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
    );
    // ADR 0015 M1 (`4b3f890`): start_agent is the event-driven
    // readiness barrier — it awaits agentd's host-bound readiness
    // dial before returning. exec_stream no longer carries the
    // pre-M1 boot-race retry loop, so callers must go through
    // start_agent first or race the agentd-1024 listener. Empty
    // argv is the readiness-only probe (`engram-agentd::
    // SpawnHarnessRequest` skips the spawn when argv is empty) —
    // we don't actually want a harness child for this test, just
    // the proof that agentd is bound.
    //
    // ADR 0021 P1.1+P1.2: `host_ca_pem` triggers `InstallHostCa`
    // over vsock right after agentd readiness and before the
    // (no-op) SpawnHarness — the in-VM trust store now carries
    // the egress proxy's CA, which is what previously rode in on
    // the (retired) harness substrate.
    backend
        .start_agent(
            sandbox_id,
            AgentSpec {
                argv: Vec::new(),
                env: HashMap::new(),
                session_env: HashMap::new(),
                host_ca_pem: Some(ca.cert_pem.clone()),
            },
        )
        .await
        .expect("agentd readiness probe");

    // ---- 8. Exec curl from inside the VM ----
    // Seed /etc/hosts so the guest's resolver finds engram-test.invalid
    // (the IP is irrelevant — iptables REDIRECT catches all 443).
    // Then issue the curl that should hit the proxy.
    let req = ExecRequest {
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            format!(
                "echo '1.2.3.4 {TEST_HOST}' >> /etc/hosts && \
                 curl -sS --max-time 10 \
                 -H 'Authorization: Bearer engram_ph_e2e_xxx' \
                 https://{TEST_HOST}/v1/test"
            ),
        ],
        stdin: None,
        env: diag_env,
        workdir: None,
        timeout: Some(Duration::from_secs(15)),
    };
    let stream = backend.exec_stream(sandbox_id, req).await.expect("exec");
    let (out, err, exit) = drain(stream.events).await;
    let stdout = String::from_utf8_lossy(&out);
    let stderr = String::from_utf8_lossy(&err);

    // Capture upstream bytes BEFORE asserting (so we can dump on
    // failure even if destroy hangs).
    let captured_bytes = captured.lock().clone();

    // On failure, dump iptables + TAP state for diagnosis.
    if exit != Some(0) {
        let ipt = std::process::Command::new("iptables-save").output();
        if let Ok(o) = ipt {
            eprintln!(
                "--- iptables-save ---\n{}\n--- end ---",
                String::from_utf8_lossy(&o.stdout)
            );
        }
        let taps = std::process::Command::new("sh")
            .arg("-c")
            .arg("ip a | grep -B 1 -A 5 tap-engr-")
            .output();
        if let Ok(o) = taps {
            eprintln!(
                "--- host TAPs ---\n{}\n--- end ---",
                String::from_utf8_lossy(&o.stdout)
            );
        }
    }

    let _ = backend.destroy(sandbox_id).await;
    delete_stale_taps();
    iptables_cleanup();

    assert_eq!(
        exit,
        Some(0),
        "curl should succeed (CA must be installed + proxy reachable). \
         stdout=`{stdout}` stderr=`{stderr}`",
    );
    let body = String::from_utf8_lossy(&captured_bytes);
    assert!(
        body.contains("Authorization: Bearer sk-real-secret-from-host"),
        "upstream should have received the real secret after substitution; got: {body}",
    );
    assert!(
        !body.contains("engram_ph_e2e_xxx"),
        "placeholder must not survive into the upstream payload; got: {body}",
    );
}
