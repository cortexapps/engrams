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

use engram_core::SessionId;
use engram_egress_proxy::ca::Ca;
use engram_egress_proxy::cert_mint::CertMint;
use engram_egress_proxy::intercept::{
    self, build_client_config, build_server_config, InterceptError,
};
use engram_egress_proxy::observe::{ObserveSink, ObservedAsset};
use engram_egress_proxy::policy::HostList;
use engram_egress_proxy::registry::{
    InjectEntry, ObserveEntry, RequestPolicy, SecretEntry, SuccessRule,
};
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

/// TLS-handshake to the proxy as if it were `fake-upstream`, trusting the
/// engram CA so the proxy-minted leaf validates. (Factored from the inline
/// setup the substitution tests use.)
async fn tls_client_to(
    ca: &Ca,
    client_to_proxy: tokio::io::DuplexStream,
) -> tokio_rustls::client::TlsStream<tokio::io::DuplexStream> {
    let pem = ca.cert_pem.clone();
    let mut roots = rustls::RootCertStore::empty();
    let cert_der = rustls_pemfile::certs(&mut pem.as_bytes())
        .next()
        .unwrap()
        .unwrap();
    roots.add(cert_der).unwrap();
    let cli_cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(cli_cfg));
    let server_name: rustls::pki_types::ServerName<'static> = "fake-upstream".try_into().unwrap();
    connector
        .connect(server_name, client_to_proxy)
        .await
        .unwrap()
}

// ADR 0056: a Plane-B injection allowing `GET /api/v2/logs*` on
// fake-upstream, adding `DD-API-KEY: <secret>`.
fn inject_entry(secret: &str, methods: &[&str], paths: &[&str]) -> InjectEntry {
    InjectEntry {
        secret: secret.into(),
        header_name: "DD-API-KEY".into(),
        header_template: "{}".into(),
        allow: HostList::from_manifest(&["fake-upstream".into()], &[]).unwrap(),
        policy: RequestPolicy {
            methods: methods.iter().map(|s| s.to_string()).collect(),
            path_prefixes: paths.iter().map(|s| s.to_string()).collect(),
        },
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
        // Drain until we see the HTTP headers terminator. A single
        // `read()` is not enough — the proxy can flush the rewritten
        // request across several TLS records, and which boundary a
        // record lands on depends on scheduling. The body is empty in
        // both test cases (Content-Length: 0), so `\r\n\r\n` marks the
        // full request.
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        loop {
            match tls.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        captured.lock().extend_from_slice(&buf);
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
    let resolver = Arc::new(StaticResolver::new().with("fake-upstream", upstream_addr));
    let proxy_task = tokio::spawn(async move {
        let secrets: Vec<&SecretEntry> = vec![&secret];
        intercept::run(
            proxy_from_client,
            Vec::new(),
            "fake-upstream",
            upstream_addr.port(),
            resolver,
            &secrets,
            &[],
            &[],
            SessionId::new(),
            None,
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
    let server_name: rustls::pki_types::ServerName<'static> = "fake-upstream".try_into().unwrap();
    let mut tls_client = connector
        .connect(server_name, client_to_proxy)
        .await
        .unwrap();

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
    let resolver = Arc::new(StaticResolver::new().with("fake-upstream", upstream_addr));
    let proxy_task = tokio::spawn(async move {
        let secrets: Vec<&SecretEntry> = vec![&secret];
        intercept::run(
            proxy_from_client,
            Vec::new(),
            "fake-upstream",
            upstream_addr.port(),
            resolver,
            &secrets,
            &[],
            &[],
            SessionId::new(),
            None,
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
    let server_name: rustls::pki_types::ServerName<'static> = "fake-upstream".try_into().unwrap();
    let mut tls_client = connector
        .connect(server_name, client_to_proxy)
        .await
        .unwrap();
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

// ADR 0056: an allowed request shape gets the credential header injected;
// the guest never sent (and never holds) the secret.
#[tokio::test]
async fn injects_header_on_allowed_request() {
    let ca = ca();
    let mint = Arc::new(CertMint::new(ca.clone()));
    let server_cfg = build_server_config(mint);
    let client_cfg = build_client_config();

    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream_addr = fake_upstream(captured.clone()).await;
    let (client_to_proxy, proxy_from_client) = tokio::io::duplex(64 * 1024);

    let inj = inject_entry("dd-secret-xyz", &["GET"], &["/api/v2/logs*"]);
    let resolver = Arc::new(StaticResolver::new().with("fake-upstream", upstream_addr));
    let proxy_task = tokio::spawn(async move {
        let injects: Vec<&InjectEntry> = vec![&inj];
        intercept::run(
            proxy_from_client,
            Vec::new(),
            "fake-upstream",
            upstream_addr.port(),
            resolver,
            &[],
            &injects,
            &[],
            SessionId::new(),
            None,
            server_cfg,
            client_cfg,
        )
        .await
    });

    let mut tls_client = tls_client_to(&ca, client_to_proxy).await;
    tls_client
        .write_all(
            b"GET /api/v2/logs/events HTTP/1.1\r\nHost: fake-upstream\r\nContent-Length: 0\r\n\r\n",
        )
        .await
        .unwrap();
    tls_client.flush().await.unwrap();
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
        body.contains("DD-API-KEY: dd-secret-xyz\r\n"),
        "upstream should have seen the injected credential header; got: {body}",
    );
    assert!(
        body.starts_with("GET /api/v2/logs/events HTTP/1.1\r\n"),
        "request line preserved",
    );
}

// ADR 0056: a request to an inject-gated host whose (method, path) matches
// no policy is rejected — the secret is never injected and nothing reaches
// upstream.
#[tokio::test]
async fn rejects_request_shape_outside_policy() {
    let ca = ca();
    let mint = Arc::new(CertMint::new(ca.clone()));
    let server_cfg = build_server_config(mint);
    let client_cfg = build_client_config();

    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream_addr = fake_upstream(captured.clone()).await;
    let (client_to_proxy, proxy_from_client) = tokio::io::duplex(64 * 1024);

    // Only GET /api/v2/logs* is allowed; the client tries POST /api/v2/metrics.
    let inj = inject_entry("dd-secret-xyz", &["GET"], &["/api/v2/logs*"]);
    let resolver = Arc::new(StaticResolver::new().with("fake-upstream", upstream_addr));
    let proxy_task = tokio::spawn(async move {
        let injects: Vec<&InjectEntry> = vec![&inj];
        intercept::run(
            proxy_from_client,
            Vec::new(),
            "fake-upstream",
            upstream_addr.port(),
            resolver,
            &[],
            &injects,
            &[],
            SessionId::new(),
            None,
            server_cfg,
            client_cfg,
        )
        .await
    });

    let mut tls_client = tls_client_to(&ca, client_to_proxy).await;
    tls_client
        .write_all(
            b"POST /api/v2/metrics HTTP/1.1\r\nHost: fake-upstream\r\nContent-Length: 0\r\n\r\n",
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
        matches!(outcome, Err(InterceptError::RequestRejected { .. })),
        "expected RequestRejected, got: {outcome:?}",
    );
    assert!(
        captured.lock().is_empty(),
        "upstream must see nothing when the request shape is rejected",
    );
}

/// A TLS upstream that captures the request head, then replies with the given
/// response bytes and closes. Used by the observe tests to return a JSON body.
async fn fake_upstream_resp(captured: Arc<Mutex<Vec<u8>>>, response: Vec<u8>) -> SocketAddr {
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
        let mut buf = Vec::new();
        let mut tmp = [0u8; 8192];
        loop {
            match tls.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        captured.lock().extend_from_slice(&buf);
        let _ = tls.write_all(&response).await;
        let _ = tls.shutdown().await;
    });
    addr
}

// ADR 0056 Phase 4: a request matching an observe spec gets its response parsed
// and an IntegrationAsset emitted to the sink — built from the REAL response
// bytes, never a guest claim.
#[tokio::test]
async fn observes_response_and_emits_asset() {
    let ca = ca();
    let mint = Arc::new(CertMint::new(ca.clone()));
    let server_cfg = build_server_config(mint);
    let client_cfg = build_client_config();

    let body = r#"{"number":42,"title":"Bug","html_url":"http://x/i/42"}"#;
    let response = format!(
        "HTTP/1.1 201 Created\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes();
    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream_addr = fake_upstream_resp(captured.clone(), response).await;
    let (client_to_proxy, proxy_from_client) = tokio::io::duplex(64 * 1024);

    let observe = ObserveEntry {
        allow: HostList::from_manifest(&["fake-upstream".into()], &[]).unwrap(),
        policy: RequestPolicy {
            methods: vec!["POST".into()],
            path_prefixes: vec!["/repos/*/issues".into()],
        },
        provider: "github".into(),
        asset_kind: "issue".into(),
        surface: "asset".into(),
        success: SuccessRule::StatusClass2xx,
        data: vec![
            ("number".into(), "$.resp.number".into()),
            ("title".into(), "$.resp.title".into()),
        ],
        fetchable: Some("$.resp.html_url".into()),
    };

    let collected: Arc<Mutex<Vec<(SessionId, ObservedAsset)>>> = Arc::new(Mutex::new(Vec::new()));
    let collected_for_sink = collected.clone();
    let sink: ObserveSink =
        Arc::new(move |sid, asset| collected_for_sink.lock().push((sid, asset)));
    let session_id = SessionId::new();

    let resolver = Arc::new(StaticResolver::new().with("fake-upstream", upstream_addr));
    let proxy_task = tokio::spawn(async move {
        let observes: Vec<&ObserveEntry> = vec![&observe];
        intercept::run(
            proxy_from_client,
            Vec::new(),
            "fake-upstream",
            upstream_addr.port(),
            resolver,
            &[],
            &[],
            &observes,
            session_id,
            Some(&sink),
            server_cfg,
            client_cfg,
        )
        .await
    });

    let mut tls_client = tls_client_to(&ca, client_to_proxy).await;
    tls_client
        .write_all(
            b"POST /repos/x/issues HTTP/1.1\r\nHost: fake-upstream\r\nAccept-Encoding: gzip\r\nContent-Length: 0\r\n\r\n",
        )
        .await
        .unwrap();
    tls_client.flush().await.unwrap();
    // Read the forwarded response so the client side completes + closes.
    let mut resp = Vec::new();
    let _ = tls_client.read_to_end(&mut resp).await;
    let _ = tls_client.shutdown().await;
    drop(tls_client);

    let outcome = proxy_task.await.unwrap();
    if let Err(e) = &outcome {
        let msg = format!("{e}");
        if !msg.contains("close_notify") && !msg.contains("UnexpectedEof") {
            panic!("proxy returned unexpected error: {e}");
        }
    }

    // The client received the real upstream response (forwarded unchanged).
    assert!(
        String::from_utf8_lossy(&resp).contains("\"number\":42"),
        "client should see the forwarded response; got: {}",
        String::from_utf8_lossy(&resp),
    );
    // Upstream saw the request with Accept-Encoding stripped + Connection: close.
    let req_seen = String::from_utf8_lossy(&captured.lock().clone()).to_string();
    assert!(!req_seen.to_ascii_lowercase().contains("accept-encoding"));
    assert!(req_seen.contains("Connection: close\r\n"));

    // The asset was emitted from the real response bytes.
    let got = collected.lock().clone();
    assert_eq!(got.len(), 1, "exactly one asset emitted");
    let (sid, asset) = &got[0];
    assert_eq!(*sid, session_id);
    assert_eq!(asset.provider, "github");
    assert_eq!(asset.asset_kind, "issue");
    assert_eq!(asset.surface, "asset");
    assert_eq!(asset.data.get("number"), Some(&serde_json::json!(42)));
    assert_eq!(asset.data.get("title"), Some(&serde_json::json!("Bug")));
    assert_eq!(asset.fetchable_url.as_deref(), Some("http://x/i/42"));
}

// ADR 0056 Phase 4: a non-2xx response on a marked endpoint emits NO asset
// (the side effect did not occur) — the success rule gates it.
#[tokio::test]
async fn failed_status_emits_no_asset() {
    let ca = ca();
    let mint = Arc::new(CertMint::new(ca.clone()));
    let server_cfg = build_server_config(mint);
    let client_cfg = build_client_config();

    let response = b"HTTP/1.1 422 Unprocessable Entity\r\nContent-Length: 2\r\n\r\n{}".to_vec();
    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream_addr = fake_upstream_resp(captured.clone(), response).await;
    let (client_to_proxy, proxy_from_client) = tokio::io::duplex(64 * 1024);

    let observe = ObserveEntry {
        allow: HostList::from_manifest(&["fake-upstream".into()], &[]).unwrap(),
        policy: RequestPolicy {
            methods: vec!["POST".into()],
            path_prefixes: vec!["/repos/*/issues".into()],
        },
        provider: "github".into(),
        asset_kind: "issue".into(),
        surface: "asset".into(),
        success: SuccessRule::StatusClass2xx,
        data: vec![("number".into(), "$.resp.number".into())],
        fetchable: None,
    };

    let collected: Arc<Mutex<Vec<(SessionId, ObservedAsset)>>> = Arc::new(Mutex::new(Vec::new()));
    let collected_for_sink = collected.clone();
    let sink: ObserveSink =
        Arc::new(move |sid, asset| collected_for_sink.lock().push((sid, asset)));

    let resolver = Arc::new(StaticResolver::new().with("fake-upstream", upstream_addr));
    let proxy_task = tokio::spawn(async move {
        let observes: Vec<&ObserveEntry> = vec![&observe];
        intercept::run(
            proxy_from_client,
            Vec::new(),
            "fake-upstream",
            upstream_addr.port(),
            resolver,
            &[],
            &[],
            &observes,
            SessionId::new(),
            Some(&sink),
            server_cfg,
            client_cfg,
        )
        .await
    });

    let mut tls_client = tls_client_to(&ca, client_to_proxy).await;
    tls_client
        .write_all(
            b"POST /repos/x/issues HTTP/1.1\r\nHost: fake-upstream\r\nContent-Length: 0\r\n\r\n",
        )
        .await
        .unwrap();
    tls_client.flush().await.unwrap();
    let mut resp = Vec::new();
    let _ = tls_client.read_to_end(&mut resp).await;
    let _ = tls_client.shutdown().await;
    drop(tls_client);
    let _ = proxy_task.await.unwrap();

    assert!(
        collected.lock().is_empty(),
        "a 422 must not emit an asset (the effect did not occur)",
    );
}
