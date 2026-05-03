//! End-to-end MITM test using only in-process tokio sockets.
//!
//! Spins up a tiny TLS upstream that captures requests, runs the
//! proxy's `intercept::run` against a client connection, and
//! asserts:
//!   - A placeholder gets substituted for its real value when the
//!     destination is allowed.
//!   - Violation is reported when the placeholder is sent to a
//!     destination outside the secret's `allow_hosts`.
//!
//! No real networking — everything is loopback.

use std::net::SocketAddr;
use std::sync::Arc;

use engram_egress_proxy::ca::Ca;
use engram_egress_proxy::cert_mint::CertMint;
use engram_egress_proxy::intercept::{self, build_client_config, build_server_config, InterceptError};
use engram_egress_proxy::policy::HostList;
use engram_egress_proxy::registry::SecretEntry;
use engram_egress_proxy::resolver::StaticResolver;
use parking_lot::Mutex;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::TlsConnector;

fn ca() -> Arc<Ca> {
    let tmp = tempfile::tempdir().unwrap().keep();
    Arc::new(Ca::load_or_generate(&tmp).unwrap())
}

fn entry(placeholder: &str, real: &str, allow: &[&str]) -> SecretEntry {
    SecretEntry {
        placeholder: placeholder.into(),
        real_value: real.into(),
        allow: HostList::from_manifest(
            &allow.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &[],
        )
        .unwrap(),
    }
}

/// Spin up a TLS upstream that:
///   - Self-signs its own cert for "fake-upstream"
///   - Accepts one connection
///   - Reads the entire request body, stuffs it into the captured
///     Mutex, sends a 200 OK, closes
async fn fake_upstream(captured: Arc<Mutex<Vec<u8>>>) -> SocketAddr {
    // Self-signed leaf for "fake-upstream"; the proxy's client
    // config skips verification, so the cert chain doesn't matter.
    let mut params = CertificateParams::new(vec!["fake-upstream".to_string()]).unwrap();
    params.distinguished_name = {
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "fake-upstream");
        dn
    };
    params.subject_alt_names = vec![SanType::DnsName("fake-upstream".try_into().unwrap())];
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
        let (stream, _) = listener.accept().await.unwrap();
        let mut tls = acceptor.accept(stream).await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = tls.read(&mut buf).await.unwrap();
        captured.lock().extend_from_slice(&buf[..n]);
        let _ = tls
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await;
        let _ = tls.shutdown().await;
    });
    addr
}

#[tokio::test]
async fn substitutes_placeholder_in_intercept_path() {
    let ca = ca();
    let mint = Arc::new(CertMint::new(ca.clone()));
    let server_cfg = build_server_config(mint);
    let client_cfg = build_client_config();

    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream_addr = fake_upstream(captured.clone()).await;

    // Client: connect to a localhost listener that the proxy
    // accepts on. We use a duplex pipe to avoid a third TCP
    // listener — the proxy's `intercept::run` takes any
    // AsyncRead+AsyncWrite, so loopback bytes work.
    let (client_to_proxy, proxy_from_client) = tokio::io::duplex(64 * 1024);

    let secret = entry("engram_ph_xxx_yyy", "sk-real", &["fake-upstream"]);
    let server_cfg_for_task = server_cfg.clone();
    let client_cfg_for_task = client_cfg.clone();
    let resolver = Arc::new(
        StaticResolver::new().with("fake-upstream", upstream_addr),
    );
    let proxy_task = tokio::spawn(async move {
        let secrets: Vec<&SecretEntry> = vec![&secret];
        intercept::run(
            proxy_from_client,
            Vec::new(),
            "fake-upstream",
            upstream_addr.port(),
            resolver,
            &secrets,
            server_cfg_for_task,
            client_cfg_for_task,
        )
        .await
    });

    // Client side: TLS-handshake to the proxy as if it were
    // fake-upstream. We trust the engram CA so the leaf the proxy
    // mints for us validates.
    let cert_pem = ca.cert_pem.clone();
    let mut roots = rustls::RootCertStore::empty();
    let cert_der = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .next()
        .unwrap()
        .unwrap();
    roots.add(cert_der).unwrap();
    let cli_cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(cli_cfg));
    let server_name: rustls::pki_types::ServerName<'static> =
        "fake-upstream".try_into().unwrap();
    let mut tls_client = connector.connect(server_name, client_to_proxy).await.unwrap();

    tls_client
        .write_all(
            b"POST /v1/chat HTTP/1.1\r\nHost: fake-upstream\r\nAuthorization: Bearer engram_ph_xxx_yyy\r\nContent-Length: 0\r\n\r\n",
        )
        .await
        .unwrap();
    tls_client.flush().await.unwrap();

    // Read response so the proxy task can complete. Tolerate
    // UnexpectedEof and propagate failures of the proxy task only
    // if they're not the close_notify-missing flavor — we care
    // about substitution correctness here, not perfect TLS
    // shutdown choreography (which is finicky to get clean across
    // tokio duplex pipes).
    let mut resp = [0u8; 1024];
    let _ = tls_client.read(&mut resp).await;
    let _ = tls_client.shutdown().await;
    drop(tls_client);

    let outcome = proxy_task.await.unwrap();
    if let Err(e) = &outcome {
        let msg = format!("{e}");
        if !msg.contains("close_notify") && !msg.contains("UnexpectedEof") {
            panic!("proxy returned unexpected error: {e}");
        }
    }

    let body = String::from_utf8(captured.lock().clone()).unwrap();
    assert!(
        body.contains("Authorization: Bearer sk-real"),
        "upstream should have seen the real token after substitution; got: {body}",
    );
    assert!(
        !body.contains("engram_ph_xxx_yyy"),
        "placeholder must not survive into the upstream payload",
    );
}

#[tokio::test]
async fn violation_returned_when_placeholder_targets_disallowed_host() {
    let ca = ca();
    let mint = Arc::new(CertMint::new(ca.clone()));
    let server_cfg = build_server_config(mint);
    let client_cfg = build_client_config();

    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream_addr = fake_upstream(captured.clone()).await;

    let (client_to_proxy, proxy_from_client) = tokio::io::duplex(64 * 1024);

    // Secret allows api.openai.com — but we send it to fake-upstream,
    // which IS allowed by network.allow_hosts (so we Intercept) but
    // NOT by the secret's allow_hosts (so substitution skips it
    // and the violation scanner sees the placeholder).
    let secret = entry("engram_ph_xxx_yyy", "sk-real", &["api.openai.com"]);
    let resolver = Arc::new(
        StaticResolver::new().with("fake-upstream", upstream_addr),
    );
    let proxy_task = tokio::spawn(async move {
        let secrets: Vec<&SecretEntry> = vec![&secret];
        intercept::run(
            proxy_from_client,
            Vec::new(),
            "fake-upstream",
            upstream_addr.port(),
            resolver,
            &secrets,
            server_cfg,
            client_cfg,
        )
        .await
    });

    let cert_pem = ca.cert_pem.clone();
    let mut roots = rustls::RootCertStore::empty();
    let cert_der = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .next()
        .unwrap()
        .unwrap();
    roots.add(cert_der).unwrap();
    let cli_cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(cli_cfg));
    let server_name: rustls::pki_types::ServerName<'static> =
        "fake-upstream".try_into().unwrap();
    let mut tls_client = connector.connect(server_name, client_to_proxy).await.unwrap();
    tls_client
        .write_all(
            b"POST / HTTP/1.1\r\nHost: fake-upstream\r\nAuthorization: Bearer engram_ph_xxx_yyy\r\n\r\n",
        )
        .await
        .unwrap();
    tls_client.flush().await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut buf = [0u8; 1024];
        let _ = tls_client.read(&mut buf).await;
    })
    .await;

    let outcome = proxy_task.await.unwrap();
    assert!(
        matches!(outcome, Err(InterceptError::Violation { .. })),
        "expected Violation, got: {outcome:?}",
    );
    // Upstream must NOT have received the placeholder (proxy closes
    // before forwarding).
    assert!(
        captured.lock().is_empty(),
        "upstream should not see any bytes when violation fires",
    );
}
