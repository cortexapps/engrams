//! End-to-end of the warm-restore egress chain:
//!
//!   netns process → tap-engr-XXX (in netns) → netns SNAT → veth →
//!   root vh-engr-XXX → iptables REDIRECT (root) → engram-egress-proxy
//!
//! This is the warm-path complement to `proxy_e2e.rs` (cold-path,
//! TAP-in-root). It validates that the prod 2026-05-20 cascade of
//! bugs is actually closed:
//!
//!   - Fix 1 (iptables vh-engr-+ REDIRECT, this repo's 9784abf):
//!     a warm-path packet's destination on tcp/443 gets REDIRECT'd
//!     to the host's proxy port. Without this, the packet falls
//!     through to the default-deny FORWARD DROP and is blackholed.
//!
//!   - Fix 3 (host overwrites policy.guest_ip with the SNAT'd IP):
//!     once the packet reaches the proxy, the proxy's `peer.ip()`
//!     is the netns SNAT slot (e.g. 10.200.0.6), and the proxy's
//!     `Registry::lookup` MUST find a session keyed on that IP.
//!     Pre-fix the registry was keyed on `Ipv4Addr::UNSPECIFIED`
//!     because coord ships the placeholder; the proxy dropped
//!     every connection with "no session for source IP".
//!
//!   - SO_ORIGINAL_DST recovery through the netns→root dual-NAT:
//!     netns POSTROUTING SNAT (source rewrite) followed by root
//!     PREROUTING REDIRECT (dest rewrite). The proxy's
//!     `socket2::SockRef::original_dst_v4` must return the
//!     pre-REDIRECT dest (e.g. 192.0.2.42:443) so the proxy can
//!     route by SNI to the right upstream.
//!
//! This test does NOT bake a rootfs or boot an FC microVM — that's
//! `proxy_e2e.rs`'s job for the cold path. We exercise the network
//! layer in isolation using `ip netns exec bash` to fire real TCP
//! dials from inside the netns. From iptables / conntrack / the
//! proxy's perspective the packet is indistinguishable from one
//! sourced by a guest VM's eth0.
//!
//! Run with sudo on the dev VM:
//!
//!   sudo -E env "PATH=$PATH" cargo test \
//!       -p engram-sandbox-firecracker --test warm_proxy_chain -- --ignored
//!
//! Like the other ignore'd live-iptables tests in this crate, this
//! one mutates host iptables and netns state under root. It cleans
//! up on success; on a panic it leaves the rules in place so the
//! reader can inspect them via `iptables-save`.

#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use engram_core::SandboxId;
use engram_sandbox_firecracker::net::{
    host_startup, netns_name_for, provision_netns, tap_name_for, teardown_netns, NetworkAllocator,
    VmCidr,
};
use parking_lot::Mutex;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// SNI / hostname the test pretends the netns is dialing. `.invalid`
/// per RFC 2606 — guaranteed not to resolve via real DNS. The
/// proxy's StaticResolver maps it to the local fake upstream; the
/// netns dialer uses raw IP `192.0.2.42` (TEST-NET-1 per RFC 5737),
/// which doesn't actually need to resolve since the REDIRECT
/// captures any dport=443.
const TEST_HOST: &str = "engram-warm-test.invalid";

/// TEST-NET-1 IP for the netns dialer. Routes via the netns's
/// default gateway → veth → root, where the REDIRECT catches it.
const TEST_DEST_IP: &str = "192.0.2.42";

fn require_root() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    let euid: u32 = status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(2).and_then(|s| s.parse().ok()))
        .expect("parse euid");
    if euid != 0 {
        eprintln!(
            "SKIP: warm_proxy_chain requires root (CAP_NET_ADMIN for netns + iptables). \
             Re-run via `sudo -E ...`."
        );
        return false;
    }
    true
}

/// Spin up a TLS upstream that captures the first request and
/// replies 200 OK. Mirrors `proxy_e2e::fake_upstream`.
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

fn cleanup_iptables() {
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
#[ignore = "requires Linux + root (CAP_NET_ADMIN for netns + iptables); run with sudo"]
async fn warm_path_redirects_through_proxy_with_correct_source_lookup() {
    if !require_root() {
        return;
    }
    cleanup_iptables();

    // ---- 1. Proxy + Registry + StaticResolver + fake upstream ----
    let proxy_dir = tempfile::tempdir().unwrap();
    let ca = Arc::new(engram_egress_proxy::Ca::load_or_generate(proxy_dir.path()).unwrap());
    let _ = rustls::crypto::ring::default_provider().install_default();
    let registry = Arc::new(engram_egress_proxy::Registry::new());
    let mint = Arc::new(engram_egress_proxy::CertMint::new(ca.clone()));

    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream_addr = fake_upstream(captured.clone()).await;
    let resolver =
        Arc::new(engram_egress_proxy::StaticResolver::new().with(TEST_HOST, upstream_addr));

    // Unusual port so collisions with other test processes are
    // impossible. Matched by the host_startup REDIRECT below.
    let proxy_port: u16 = 28443;
    let proxy_bind: SocketAddr = format!("0.0.0.0:{proxy_port}").parse().unwrap();
    let mut proxy_cfg = engram_egress_proxy::ProxyConfig::new(proxy_bind, registry.clone(), mint);
    proxy_cfg.resolver = resolver;
    proxy_cfg.dns_bind_addr = None; // we don't exercise DNS in this test
    let proxy = engram_egress_proxy::Proxy::new(proxy_cfg);
    // Bind synchronously (ADR 0083) — listener up before serve spawns.
    let listeners = proxy.bind().await.expect("egress proxy bind");
    tokio::spawn(async move {
        proxy.serve(listeners).await;
    });

    // ---- 2. Install the host iptables ruleset (Fix 1 lives here) ----
    host_startup(Some(proxy_port), None, None)
        .await
        .expect("host_startup");

    // ---- 3. Provision a per-VM netns just like warm-restore ----
    let sandbox_id = SandboxId::new();
    let bake_cidr = VmCidr::new("10.200.0.0".parse().unwrap());
    let tap_name = tap_name_for(sandbox_id);
    let allocator = Mutex::new(NetworkAllocator::new("10.200.0.0".parse().unwrap()));
    let setup = provision_netns(sandbox_id, bake_cidr, &tap_name, &allocator)
        .await
        .expect("provision_netns");

    // ---- 4. Register the session in the proxy registry under the
    //         REAL SNAT'd guest_ip (Fix 3). Include a dummy
    //         SecretEntry whose `allow` list contains TEST_HOST —
    //         that makes `SessionState::decide(TEST_HOST)` return
    //         `Decision::Intercept(secrets)` instead of `Bypass`,
    //         so the proxy TERMINATES TLS at its own listener and
    //         mints a leaf signed by the engram CA. The leaf chains
    //         cleanly to `ca.pem` and openssl's `-CAfile` then
    //         verifies the chain. (Bypass mode would relay bytes
    //         transparently and the netns client would see the
    //         upstream's self-signed cert, breaking verification —
    //         even though everything would still be functionally
    //         correct.) The secret's `real_value` never gets
    //         substituted in this test because openssl s_client
    //         doesn't send the placeholder; we just need
    //         Decision::Intercept.
    let session_id = engram_core::SessionId::new();
    let snat_ip = setup.snat_cidr.guest();
    let network_allow =
        engram_egress_proxy::HostList::from_manifest(&[TEST_HOST.into()], &[]).unwrap();
    let dummy_secret = engram_egress_proxy::SecretEntry {
        placeholder: "warm_proxy_chain_placeholder".into(),
        real_value: "unused-in-this-test".into(),
        allow: engram_egress_proxy::HostList::from_manifest(&[TEST_HOST.into()], &[]).unwrap(),
    };
    registry.register(engram_egress_proxy::SessionState {
        session_id,
        guest_ip: snat_ip,
        network_allow,
        allow_all: false,
        secrets: vec![dummy_secret],
        injects: Vec::new(),
        observes: Vec::new(),
    });

    // ---- 5. From inside the netns, dial TEST_DEST_IP:443 with a
    //         TLS-shaped ClientHello so the proxy's SNI peek
    //         succeeds. We use openssl(1) — broadly available in
    //         nix's coreutils-ish set on the dev VM, and lets us
    //         control SNI explicitly. Bash's /dev/tcp can't send
    //         TLS, so it's insufficient here.
    //
    //         Skip cleanly if openssl isn't on PATH (other CI
    //         shapes might not have it; the dev VM does).
    let openssl_ok = std::process::Command::new("openssl")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !openssl_ok {
        eprintln!("SKIP: openssl(1) not on PATH; can't fire a TLS ClientHello from the netns");
        teardown_netns(&setup, &allocator).await;
        cleanup_iptables();
        return;
    }

    let netns = netns_name_for(sandbox_id);
    // Point openssl at the engram CA so verification actually
    // succeeds. The proxy's CertMint signs every leaf with this CA,
    // so a successful `Verify return code: 0 (ok)` from openssl is
    // the strongest signal we can ask for: REDIRECT delivered the
    // connection, the proxy looked up the source IP, peeked SNI,
    // minted a leaf, and the leaf chains cleanly to the CA we just
    // generated. The netns shares the host filesystem (only the
    // net namespace is isolated), so the host path resolves
    // unchanged from inside `ip netns exec`.
    let ca_pem = proxy_dir.path().join("ca.pem");
    assert!(
        ca_pem.exists(),
        "engram CA didn't write to the expected path; got: {}",
        ca_pem.display(),
    );
    let openssl_cmd = format!(
        "echo | openssl s_client \
         -connect {TEST_DEST_IP}:443 \
         -servername {TEST_HOST} \
         -CAfile {ca_path} \
         2>&1 | head -c 2048",
        ca_path = ca_pem.display(),
    );
    let dial = tokio::process::Command::new("ip")
        .args(["netns", "exec", &netns, "bash", "-c", &openssl_cmd])
        .output();

    let dial_res = tokio::time::timeout(Duration::from_secs(10), dial).await;

    // Tear down BEFORE asserting so a panic doesn't leak kernel state.
    teardown_netns(&setup, &allocator).await;
    cleanup_iptables();

    let dial_output = dial_res
        .expect("openssl s_client did not finish within 10s — REDIRECT or proxy hung")
        .expect("spawn openssl");
    let stderr = String::from_utf8_lossy(&dial_output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&dial_output.stdout).to_string();
    let combined = format!("{stdout}{stderr}");

    // ---- 6. The upstream should have captured the request body
    //         (HTTP-over-TLS, even if the GET was synthetic).
    //         openssl s_client just opens the connection; it
    //         doesn't send a body, so the body capture may be
    //         empty — but the TLS handshake completing means
    //         the proxy successfully:
    //           (a) recovered SO_ORIGINAL_DST → 192.0.2.42:443
    //           (b) did Registry::lookup(snat_ip) → Some(session)
    //           (c) matched SNI engram-warm-test.invalid against
    //               the session's allow list → Decision::Bypass
    //           (d) resolved via StaticResolver → upstream_addr
    //           (e) relayed bytes between client and upstream.
    //
    //         Failure modes we'd see in `combined`:
    //           - "unable to load certificate" → proxy's mint cert
    //             chain broken
    //           - connection refused → REDIRECT didn't fire
    //           - handshake failure → SNI peek / cert minting bug
    //           - "no session for source IP" (in proxy logs, not
    //             stderr) → Fix 3 not applied or applied wrong
    //
    //         The proxy mints leaf certs on-demand with the SNI as
    //         the CN; openssl's `depth=0 CN=<TEST_HOST>` line in
    //         the output is the load-bearing signal — it proves
    //         openssl received a cert with the right CN, which
    //         means the full chain worked: REDIRECT → proxy →
    //         registry lookup matched → SNI peek matched →
    //         cert mint → cert presented.
    eprintln!("--- openssl s_client output ---\n{combined}\n--- end ---");
    // Strongest signal: openssl successfully verified the leaf cert
    // against the engram CA. This means: (a) REDIRECT delivered
    // the SYN, (b) the proxy's `Registry::lookup(snat_ip)` matched
    // post-Fix-3, (c) SNI peek extracted TEST_HOST from the
    // ClientHello, (d) CertMint minted a leaf with TEST_HOST as
    // the CN and signed by the engram CA, (e) TLS handshake
    // completed end-to-end, (f) chain verification passed against
    // -CAfile. Any failure mode (REDIRECT, registry, mint, etc.)
    // would surface as a non-zero verify return code or a connect
    // error before getting here.
    assert!(
        combined.contains("Verify return code: 0 (ok)"),
        "openssl did not get `Verify return code: 0 (ok)` — \
         the warm-egress chain is broken somewhere. Combined output:\n{combined}",
    );
    assert!(
        !combined.contains("connect:errno"),
        "openssl reported a connect error — REDIRECT likely didn't fire \
         or the proxy isn't bound. Output:\n{combined}",
    );
}
