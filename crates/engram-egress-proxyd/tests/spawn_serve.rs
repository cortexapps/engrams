//! ADR 0121: the daemon's core property, at process level, no KVM.
//!
//! A real `engram-egress-proxyd` child serves a real TLS stream
//! (bypass path: client ↔ upstream through the proxy splice) while
//! its control client — the stand-in for host-agent — disconnects and
//! reconnects around it. The stream must keep flowing: the daemon's
//! whole reason to exist is that the data plane outlives the control
//! plane. Also covers the connect-time `SyncPolicies` replace+prune,
//! the unregistered-peer prompt close (the adopt probe's signal), and
//! the graceful `Shutdown` op.
//!
//! Sized to the property: one upstream, one stream, two chunks — the
//! second sent only after the control plane died and came back.

use std::io::Write as _;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use engram_egress_proto::{DialbackRequest, FromProxyd, HelloInfo, ToProxyd};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Test harness drives a live child process; wall-clock waits are world
// inputs here, not decisions (ADR 0098 D1).
const WAIT_BUDGET: Duration = Duration::from_secs(15);

struct TestCa {
    cert_pem: String,
    key_pem: String,
}

fn make_ca() -> TestCa {
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    TestCa {
        cert_pem: cert.pem(),
        key_pem: key.serialize_pem(),
    }
}

/// A self-signed upstream for `example.com` and a rustls server/client
/// config pair that trusts it.
struct Upstream {
    addr: std::net::SocketAddr,
    client_roots: rustls::RootCertStore,
    /// Tell the accept task to send the second chunk.
    release_second: tokio::sync::mpsc::UnboundedSender<()>,
}

async fn spawn_upstream() -> Upstream {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["example.com".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap();

    let mut client_roots = rustls::RootCertStore::empty();
    client_roots.add(cert_der.clone()).unwrap();

    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (release_second, mut release_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut tls = acceptor.accept(tcp).await.unwrap();
        tls.write_all(b"chunk-one").await.unwrap();
        tls.flush().await.unwrap();
        // Hold the stream open until the test says the control plane
        // has died and returned, then prove it still flows.
        let _ = release_rx.recv().await;
        tls.write_all(b"chunk-two").await.unwrap();
        tls.flush().await.unwrap();
        // Keep the stream open until the client is done reading.
        let mut sink = [0u8; 16];
        let _ = tls.read(&mut sink).await;
    });

    Upstream {
        addr,
        client_roots,
        release_second,
    }
}

fn free_port() -> u16 {
    // Bind-then-drop: a small race against other tests, ridden by the
    // daemon's own bind_with_retry.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

struct Daemon {
    child: std::process::Child,
    work_dir: tempfile::TempDir,
    proxy_port: u16,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_daemon(upstream_addr: std::net::SocketAddr, ca: &TestCa) -> Daemon {
    let work_dir = tempfile::tempdir().unwrap();
    let proxy_port = free_port();
    let dns_port = free_port();
    let gateway_port = free_port();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_engram-egress-proxyd"))
        .arg("--work-dir")
        .arg(work_dir.path())
        .arg("--proxy-port")
        .arg(proxy_port.to_string())
        .arg("--dns-port")
        .arg(dns_port.to_string())
        .arg("--gateway-port")
        .arg(gateway_port.to_string())
        .arg("--test-resolve")
        .arg(format!("example.com={upstream_addr}"))
        .env(engram_egress_proto::ENV_CA_CERT_PEM, &ca.cert_pem)
        .env(engram_egress_proto::ENV_CA_KEY_PEM, &ca.key_pem)
        .env(
            engram_egress_proto::ENV_COORD_URL,
            "http://127.0.0.1:1", // never dialed in this test
        )
        .env(
            engram_egress_proto::ENV_HOST_ID,
            engram_core::HostId::new().to_string(),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    Daemon {
        child,
        work_dir,
        proxy_port,
    }
}

async fn wait_for_manifest(daemon: &Daemon) -> engram_egress_proto::manifest::ProxydManifest {
    let deadline = tokio::time::Instant::now() + WAIT_BUDGET;
    loop {
        if let Some(m) = engram_egress_proto::manifest::read_manifest(daemon.work_dir.path()) {
            return m;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon never wrote its manifest (log: {:?})",
            std::fs::read_to_string(
                daemon
                    .work_dir
                    .path()
                    .join(engram_egress_proto::LOG_FILE_NAME)
            )
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The control client — the host-agent stand-in.
struct Control {
    stream: tokio::net::UnixStream,
}

impl Control {
    async fn connect(daemon: &Daemon) -> Self {
        let sock = daemon
            .work_dir
            .path()
            .join(engram_egress_proto::CONTROL_SOCK_NAME);
        let stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
        Self { stream }
    }

    async fn roundtrip(&mut self, msg: &ToProxyd) -> FromProxyd {
        engram_egress_proto::write_frame(&mut self.stream, msg)
            .await
            .unwrap();
        engram_egress_proto::read_frame(&mut self.stream)
            .await
            .unwrap()
    }

    async fn hello(&mut self) -> HelloInfo {
        match self
            .roundtrip(&ToProxyd::Hello {
                proto_version: engram_egress_proto::PROTO_VERSION,
            })
            .await
        {
            FromProxyd::HelloAck(info) => info,
            other => panic!("expected HelloAck, got {other:?}"),
        }
    }
}

fn loopback_policy(
    session_id: engram_core::SessionId,
) -> engram_core::types::egress::SessionEgressPolicy {
    engram_core::types::egress::SessionEgressPolicy {
        session_id,
        sandbox_id: engram_core::SandboxId::new(),
        guest_ip: std::net::Ipv4Addr::LOCALHOST,
        network_allow_hosts: vec!["example.com".into()],
        network_allow_host_patterns: vec![],
        allow_all: false,
        secrets: vec![],
        injects: vec![],
        observes: vec![],
        guest_services: vec![],
        tunnels: vec![],
        secret_mode: engram_core::types::image::SecretMode::Literal,
        apps: vec![],
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_survives_control_plane_death_and_return() {
    let ca = make_ca();
    let upstream = spawn_upstream().await;
    let daemon = spawn_daemon(upstream.addr, &ca);
    let manifest = wait_for_manifest(&daemon).await;
    assert_eq!(manifest.proxy_port, daemon.proxy_port);

    // Generation A of the control plane: hello + sync.
    let mut control = Control::connect(&daemon).await;
    let hello = control.hello().await;
    assert_eq!(hello.proxy_port, daemon.proxy_port);
    let session_id = engram_core::SessionId::new();
    let reply = control
        .roundtrip(&ToProxyd::SyncPolicies(vec![loopback_policy(session_id)]))
        .await;
    assert_eq!(reply, FromProxyd::Ok);

    // The guest stand-in: TLS through the proxy's bypass splice.
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", daemon.proxy_port))
        .await
        .unwrap();
    let client_config = rustls::ClientConfig::builder()
        .with_root_certificates(upstream.client_roots.clone())
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let mut tls = connector
        .connect(
            rustls::pki_types::ServerName::try_from("example.com").unwrap(),
            tcp,
        )
        .await
        .unwrap();
    let mut buf = [0u8; 9];
    tls.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"chunk-one");

    // The roll: generation A of the control plane dies…
    drop(control);
    // …and generation B connects and re-syncs (the successor pod).
    let mut control = Control::connect(&daemon).await;
    control.hello().await;
    let reply = control
        .roundtrip(&ToProxyd::SyncPolicies(vec![loopback_policy(session_id)]))
        .await;
    assert_eq!(reply, FromProxyd::Ok);
    match control.roundtrip(&ToProxyd::Health).await {
        FromProxyd::HealthReport { sessions } => assert_eq!(sessions, 1),
        other => panic!("expected HealthReport, got {other:?}"),
    }

    // The established stream kept its sockets through all of that:
    // release the second chunk and read it on the SAME TLS session.
    upstream.release_second.send(()).unwrap();
    let mut buf = [0u8; 9];
    tls.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"chunk-two");
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_prunes_and_unregistered_peer_is_promptly_closed() {
    let ca = make_ca();
    let upstream = spawn_upstream().await;
    let daemon = spawn_daemon(upstream.addr, &ca);
    wait_for_manifest(&daemon).await;

    let mut control = Control::connect(&daemon).await;
    control.hello().await;
    let session_id = engram_core::SessionId::new();
    control
        .roundtrip(&ToProxyd::SyncPolicies(vec![loopback_policy(session_id)]))
        .await;

    // An empty sync prunes the registration (the stale-entry path).
    let reply = control.roundtrip(&ToProxyd::SyncPolicies(vec![])).await;
    assert_eq!(reply, FromProxyd::Ok);
    match control.roundtrip(&ToProxyd::Health).await {
        FromProxyd::HealthReport { sessions } => assert_eq!(sessions, 0),
        other => panic!("expected HealthReport, got {other:?}"),
    }

    // With no registration, the accept path drops the connection
    // without writing a byte — the prompt close the adopt probe keys
    // on (a NoSession peer must see EOF, never a hang).
    let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", daemon.proxy_port))
        .await
        .unwrap();
    let mut buf = [0u8; 1];
    let read = tokio::time::timeout(WAIT_BUDGET, tcp.read(&mut buf))
        .await
        .expect("unregistered peer must be closed promptly, not left hanging");
    match read {
        Ok(0) => {} // clean EOF
        Ok(n) => panic!("proxy wrote {n} bytes to an unregistered peer"),
        Err(_) => {} // RST — also a close
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_op_exits_zero_and_apply_remove_roundtrip() {
    let ca = make_ca();
    let upstream = spawn_upstream().await;
    let mut daemon = spawn_daemon(upstream.addr, &ca);
    wait_for_manifest(&daemon).await;

    let mut control = Control::connect(&daemon).await;
    control.hello().await;

    // Live apply/remove (the notify_session_policy / destroy path).
    let session_id = engram_core::SessionId::new();
    let reply = control
        .roundtrip(&ToProxyd::ApplyPolicy(Box::new(loopback_policy(
            session_id,
        ))))
        .await;
    assert_eq!(reply, FromProxyd::Ok);
    // A policy that cannot translate is refused loudly — the ack to
    // the coordinator must stay honest (ADR 0111).
    let mut bad = loopback_policy(engram_core::SessionId::new());
    bad.network_allow_host_patterns = vec!["[".into()];
    match control
        .roundtrip(&ToProxyd::ApplyPolicy(Box::new(bad)))
        .await
    {
        FromProxyd::Err(e) => assert!(e.contains("translate"), "unexpected error text: {e}"),
        other => panic!("expected Err for untranslatable policy, got {other:?}"),
    }
    let reply = control
        .roundtrip(&ToProxyd::RemoveSession(session_id))
        .await;
    assert_eq!(reply, FromProxyd::Ok);

    // Graceful shutdown: ack, then exit 0 (the RestartForUpgrade half).
    let reply = control.roundtrip(&ToProxyd::Shutdown).await;
    assert_eq!(reply, FromProxyd::Ok);
    let deadline = std::time::Instant::now() + WAIT_BUDGET;
    let status = loop {
        if let Some(status) = daemon.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "daemon did not exit after Shutdown"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(status.success(), "shutdown exit was {status:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn dialback_frames_roundtrip_over_a_socketpair() {
    // The dial-back wire shape, without a live host-agent: a server
    // stand-in accepts, reads the request, replies Ok, and passes one
    // end of a socketpair; the received fd must carry bytes.
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join(engram_egress_proto::DIALBACK_SOCK_NAME);
    let listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();

    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().unwrap();
        let req: DialbackRequest = engram_egress_proto::read_frame_sync(&mut conn).unwrap();
        assert_eq!(req.port, 4321);
        engram_egress_proto::write_frame_sync(
            &mut conn,
            &engram_egress_proto::DialbackResponse::Ok,
        )
        .unwrap();
        let (mut ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        engram_egress_proto::send_fd(&conn, std::os::fd::AsFd::as_fd(&theirs)).unwrap();
        ours.write_all(b"guest-bytes").unwrap();
    });

    let mut client = std::os::unix::net::UnixStream::connect(&sock_path).unwrap();
    engram_egress_proto::write_frame_sync(
        &mut client,
        &DialbackRequest {
            sandbox_id: engram_core::SandboxId::new(),
            port: 4321,
        },
    )
    .unwrap();
    let resp: engram_egress_proto::DialbackResponse =
        engram_egress_proto::read_frame_sync(&mut client).unwrap();
    assert_eq!(resp, engram_egress_proto::DialbackResponse::Ok);
    let fd = engram_egress_proto::recv_fd(&client).unwrap();
    let mut stream = std::os::unix::net::UnixStream::from(fd);
    let mut buf = [0u8; 11];
    std::io::Read::read_exact(&mut stream, &mut buf).unwrap();
    assert_eq!(&buf, b"guest-bytes");
    server.join().unwrap();
}
