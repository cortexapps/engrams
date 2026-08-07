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

// tests drive a live system; wall clock/OS entropy here is input, not a
// decision source (ADR 0098 D1)
#![allow(clippy::disallowed_methods)]

use std::net::SocketAddr;
use std::sync::Arc;

use engram_core::SessionId;
use engram_egress_proxy::ca::Ca;
use engram_egress_proxy::cert_mint::CertMint;
use engram_egress_proxy::intercept::{
    self, build_client_config as production_client_config, build_server_config, InterceptError,
};
use engram_egress_proxy::observe::{ObserveSink, ObservedAsset};
use engram_egress_proxy::policy::HostList;
use engram_egress_proxy::registry::{
    Decision, GraphqlMatch, GraphqlOperation, InjectEntry, InjectRefresher, ObserveEntry,
    RefreshableCred, RefreshedInject, RequestPolicy, SecretEntry, SessionState, SuccessRule,
};
use engram_egress_proxy::resolver::StaticResolver;
use parking_lot::Mutex;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::TlsConnector;

fn ca() -> Arc<Ca> {
    let tmp = tempfile::tempdir().unwrap().keep();
    Arc::new(Ca::load_or_generate(&tmp).unwrap())
}

/// Most protocol tests use a private loopback fixture. They inject a test-only
/// verifier directly into `intercept::run`; production always uses
/// `production_client_config`, which is tested separately below.
fn build_client_config() -> Arc<rustls::ClientConfig> {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};

    #[derive(Debug)]
    struct TestOnlyVerifier;
    impl ServerCertVerifier for TestOnlyVerifier {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::ECDSA_NISTP256_SHA256,
                SignatureScheme::ECDSA_NISTP384_SHA384,
                SignatureScheme::ED25519,
            ]
        }
    }

    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TestOnlyVerifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Arc::new(config)
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
    let mut cli_cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    // Exercise the production negotiation path: the guest can use H2, while
    // the HTTP/1.1 fixture upstream does not select ALPN. The proxy must mirror
    // that upstream result instead of promising H2 to this client.
    cli_cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let connector = TlsConnector::from(Arc::new(cli_cfg));
    let server_name: rustls::pki_types::ServerName<'static> = "fake-upstream".try_into().unwrap();
    let stream = connector
        .connect(server_name, client_to_proxy)
        .await
        .unwrap();
    assert_eq!(stream.get_ref().1.alpn_protocol(), None);
    stream
}

// ADR 0056: a Plane-B injection allowing `GET /api/v2/logs*` on
// fake-upstream, adding `DD-API-KEY: <secret>`.
fn inject_entry(secret: &str, methods: &[&str], paths: &[&str]) -> InjectEntry {
    InjectEntry {
        header_name: "DD-API-KEY".into(),
        header_template: "{}".into(),
        allow: HostList::from_manifest(&["fake-upstream".into()], &[]).unwrap(),
        policy: RequestPolicy {
            methods: methods.iter().map(|s| s.to_string()).collect(),
            path_globs: paths.iter().map(|s| s.to_string()).collect(),
            graphql: None,
        },
        mint_source: None,
        cred: engram_egress_proxy::RefreshableCred::new(secret.into(), None),
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
        let buf = read_http1_request(&mut tls).await;
        captured.lock().extend_from_slice(&buf);
        let _ = tls
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await;
        let _ = tls.shutdown().await;
    });
    addr
}

#[tokio::test]
async fn production_client_config_rejects_an_untrusted_upstream() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let upstream_addr = fake_upstream(captured).await;
    let connector = TlsConnector::from(production_client_config());
    let stream = TcpStream::connect(upstream_addr).await.unwrap();
    let server_name: rustls::pki_types::ServerName<'static> = "fake-upstream".try_into().unwrap();
    assert!(connector.connect(server_name, stream).await.is_err());
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
            resolver,
            intercept::StreamContext {
                sni: "fake-upstream",
                port: upstream_addr.port(),
                secrets: &secrets,
                injects: &[],
                observes: &[],
                foreign_placeholders: &[],
                session_id: SessionId::new(),
                sink: None,
                refresher: None, // WS4: no inject refresher in this test
            },
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
    let mut sink = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let _ = tls_client.read_to_end(&mut sink).await;
    })
    .await;
    drop(tls_client);

    let outcome = proxy_task.await.unwrap();
    outcome.expect("the proxy must complete a permitted request");

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

    // The secret allows api.openai.com only, and the guest sends its
    // placeholder to fake-upstream instead. fake-upstream is reachable (an
    // observe spec makes it an Intercept), so the request gets that far — and
    // the leak scan must close it.
    //
    // The whole decision comes from `SessionState::decide()`, deliberately.
    // This fixture used to hand `intercept::run` a secrets list it built
    // itself, which hid the real defect: `decide()` narrows `secrets` to the
    // HOST-MATCHING ones, so the leak scan that re-derived "which of these does
    // this host disallow?" always got an empty answer. The detector could not
    // fire in production, and this test could not notice.
    let session = SessionState {
        session_id: SessionId::new(),
        guest_ip: "10.200.0.2".parse().unwrap(),
        allow_all: false,
        network_allow: HostList::from_manifest(&["fake-upstream".into()], &[]).unwrap(),
        secrets: vec![SecretEntry {
            placeholder: "engram_ph_xxx_yyy".into(),
            real_value: "sk-real".into(),
            allow: HostList::from_manifest(&["api.openai.com".into()], &[]).unwrap(),
        }],
        injects: Vec::new(),
        observes: vec![ObserveEntry {
            allow: HostList::from_manifest(&["fake-upstream".into()], &[]).unwrap(),
            policy: RequestPolicy::default(),
            provider: "test".into(),
            asset_kind: "thing".into(),
            surface: "asset".into(),
            success: SuccessRule::StatusClass2xx,
            data: Vec::new(),
            fetchable: None,
            url_fallback: None,
        }],
        guest_services: Vec::new(),
        tunnels: Vec::new(),
    };
    let resolver = Arc::new(StaticResolver::new().with("fake-upstream", upstream_addr));
    let proxy_task = tokio::spawn(async move {
        let Decision::Intercept {
            secrets,
            injects,
            observes,
            foreign_placeholders,
        } = session.decide("fake-upstream")
        else {
            panic!("an observe spec must force an intercept");
        };
        assert!(
            secrets.is_empty(),
            "decide() narrows secrets to the host-matching ones",
        );
        assert_eq!(foreign_placeholders, vec!["engram_ph_xxx_yyy"]);
        intercept::run(
            proxy_from_client,
            Vec::new(),
            resolver,
            intercept::StreamContext {
                sni: "fake-upstream",
                port: upstream_addr.port(),
                secrets: &secrets,
                injects: &injects,
                observes: &observes,
                foreign_placeholders: &foreign_placeholders,
                session_id: session.session_id,
                sink: None,
                refresher: None, // WS4: no inject refresher in this test
            },
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
            resolver,
            intercept::StreamContext {
                sni: "fake-upstream",
                port: upstream_addr.port(),
                secrets: &[],
                injects: &injects,
                observes: &[],
                foreign_placeholders: &[],
                session_id: SessionId::new(),
                sink: None,
                refresher: None, // WS4: no inject refresher in this test
            },
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
    let mut sink = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let _ = tls_client.read_to_end(&mut sink).await;
    })
    .await;
    drop(tls_client);

    let outcome = proxy_task.await.unwrap();
    outcome.expect("the proxy must complete a permitted request");

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

// 2026-07-21 regression: the gate/inject/substitute passes see only the FIRST
// request on an intercepted connection — everything after the buffered prefix
// streams verbatim. A keep-alive client's second request therefore reached the
// upstream ungated, carrying the guest's placeholder credential (GitHub
// answered `401 Bad credentials`, breaking `gh pr create` / `gh pr checks` /
// `gh run list`, which multiplex several requests over one connection). The
// proxy must force `Connection: close` on every intercepted request so a
// compliant upstream answers once and closes — the client reconnects and every
// request gets gated + injected.
#[tokio::test]
async fn keep_alive_second_request_cannot_bypass_the_gate() {
    let ca = ca();
    let mint = Arc::new(CertMint::new(ca.clone()));
    let server_cfg = build_server_config(mint);
    let client_cfg = build_client_config();

    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    // The fake upstream honors `Connection: close`: one response, then EOF.
    let upstream_addr = fake_upstream(captured.clone()).await;
    let (client_to_proxy, proxy_from_client) = tokio::io::duplex(64 * 1024);

    let inj = inject_entry("dd-secret-xyz", &["GET"], &["/api/v2/logs*"]);
    let resolver = Arc::new(StaticResolver::new().with("fake-upstream", upstream_addr));
    let proxy_task = tokio::spawn(async move {
        let injects: Vec<&InjectEntry> = vec![&inj];
        intercept::run(
            proxy_from_client,
            Vec::new(),
            resolver,
            intercept::StreamContext {
                sni: "fake-upstream",
                port: upstream_addr.port(),
                secrets: &[],
                injects: &injects,
                observes: &[],
                foreign_placeholders: &[],
                session_id: SessionId::new(),
                sink: None,
                refresher: None,
            },
            server_cfg,
            client_cfg,
        )
        .await
    });

    let mut tls_client = tls_client_to(&ca, client_to_proxy).await;
    tls_client
        .write_all(
            b"GET /api/v2/logs/events HTTP/1.1\r\nHost: fake-upstream\r\n\
              Connection: keep-alive\r\nContent-Length: 0\r\n\r\n",
        )
        .await
        .unwrap();
    tls_client.flush().await.unwrap();

    // Read the full first response (close-delimited by the upstream).
    let mut resp = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        match tls_client.read(&mut tmp).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                resp.extend_from_slice(&tmp[..n]);
                if resp.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    assert!(
        String::from_utf8_lossy(&resp).starts_with("HTTP/1.1 200 OK"),
        "first request should succeed",
    );

    // Attempt request #2 on the same connection: a shape the policy rejects,
    // carrying a guest-held credential header. It must never reach upstream —
    // the tunnel is close-delimited, so the connection is dead by now.
    let _ = tls_client
        .write_all(
            b"GET /api/v1/admin HTTP/1.1\r\nHost: fake-upstream\r\n\
              DD-API-KEY: guest-placeholder\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
    let _ = tls_client.flush().await;
    let mut post = [0u8; 256];
    let n = tls_client.read(&mut post).await.unwrap_or(0);
    assert_eq!(
        n, 0,
        "second keep-alive request must get EOF, not a response"
    );
    let _ = tls_client.shutdown().await;
    drop(tls_client);

    let outcome = proxy_task.await.unwrap();
    if let Err(e) = &outcome {
        // The client deliberately writes request #2 into the torn-down tunnel;
        // which teardown error that surfaces is a platform/timing coin flip
        // (macOS: close_notify/UnexpectedEof, Linux: EPIPE/ECONNRESET). All of
        // them mean the same thing this test asserts: the connection died.
        let msg = format!("{e}");
        if !msg.contains("close_notify")
            && !msg.contains("UnexpectedEof")
            && !msg.contains("Broken pipe")
            && !msg.contains("Connection reset")
        {
            panic!("proxy returned unexpected error: {e}");
        }
    }

    let seen = String::from_utf8(captured.lock().clone()).unwrap();
    assert!(
        seen.contains("Connection: close\r\n"),
        "intercepted request must be rewritten to Connection: close; got: {seen}",
    );
    assert!(
        !seen.to_ascii_lowercase().contains("keep-alive"),
        "client keep-alive headers must be stripped; got: {seen}",
    );
    assert!(
        seen.contains("DD-API-KEY: dd-secret-xyz\r\n"),
        "first request still carries the injected credential; got: {seen}",
    );
    assert!(
        !seen.contains("/api/v1/admin"),
        "second request must never reach the upstream; got: {seen}",
    );
}

// PR #846 security review: a guest can't reopen the keep-alive bypass by
// decorating request #1 with `Upgrade: websocket` + `Connection: Upgrade`. A
// REST/GraphQL upstream ignores the unsupported Upgrade and would keep the
// connection persistent — but the proxy strips Upgrade and still forces
// `Connection: close`, so request #2 dies exactly as in the plain keep-alive
// case. (`fake_upstream` never speaks websockets — the attacker's upstream.)
#[tokio::test]
async fn guest_upgrade_header_cannot_reopen_the_bypass() {
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
            resolver,
            intercept::StreamContext {
                sni: "fake-upstream",
                port: upstream_addr.port(),
                secrets: &[],
                injects: &injects,
                observes: &[],
                foreign_placeholders: &[],
                session_id: SessionId::new(),
                sink: None,
                refresher: None,
            },
            server_cfg,
            client_cfg,
        )
        .await
    });

    let mut tls_client = tls_client_to(&ca, client_to_proxy).await;
    // Request #1: gated shape, but the guest tries to keep the tunnel open with
    // a websocket-upgrade dressing against a plain HTTP upstream.
    tls_client
        .write_all(
            b"GET /api/v2/logs/events HTTP/1.1\r\nHost: fake-upstream\r\n\
              Connection: Upgrade\r\nUpgrade: websocket\r\nContent-Length: 0\r\n\r\n",
        )
        .await
        .unwrap();
    tls_client.flush().await.unwrap();

    let mut resp = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        match tls_client.read(&mut tmp).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                resp.extend_from_slice(&tmp[..n]);
                if resp.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    assert!(String::from_utf8_lossy(&resp).starts_with("HTTP/1.1 200 OK"));

    // Request #2: forbidden shape carrying a guest-held credential. The forced
    // close means the connection is already dead — it must not reach upstream.
    let _ = tls_client
        .write_all(
            b"GET /api/v1/admin HTTP/1.1\r\nHost: fake-upstream\r\n\
              DD-API-KEY: guest-placeholder\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
    let _ = tls_client.flush().await;
    let mut post = [0u8; 256];
    let n = tls_client.read(&mut post).await.unwrap_or(0);
    assert_eq!(n, 0, "second request must get EOF, not a response");
    let _ = tls_client.shutdown().await;
    drop(tls_client);

    let outcome = proxy_task.await.unwrap();
    if let Err(e) = &outcome {
        let msg = format!("{e}");
        if !msg.contains("close_notify")
            && !msg.contains("UnexpectedEof")
            && !msg.contains("Broken pipe")
            && !msg.contains("Connection reset")
        {
            panic!("proxy returned unexpected error: {e}");
        }
    }

    let seen = String::from_utf8(captured.lock().clone()).unwrap();
    assert!(
        seen.contains("Connection: close\r\n"),
        "Upgrade request must still be rewritten to Connection: close; got: {seen}",
    );
    assert!(
        !seen.to_ascii_lowercase().contains("upgrade"),
        "guest-supplied Upgrade must be stripped; got: {seen}",
    );
    assert!(
        !seen.contains("/api/v1/admin"),
        "second request must never reach the upstream; got: {seen}",
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
            resolver,
            intercept::StreamContext {
                sni: "fake-upstream",
                port: upstream_addr.port(),
                secrets: &[],
                injects: &injects,
                observes: &[],
                foreign_placeholders: &[],
                session_id: SessionId::new(),
                sink: None,
                refresher: None, // WS4: no inject refresher in this test
            },
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

// WS4 (campaign C2 regression): a rejected request must NEVER surface to the
// guest as an empty success-shaped response. The reject path closes the
// connection, so the guest reads ZERO bytes (an unambiguous transport failure)
// — not a synthesized `HTTP/1.1 204`/`200` with an empty body that a stale-token
// branch DELETE could be mistaken for succeeding. (The stale-token root cause is
// fixed by the TTL-aware re-mint; this pins the shape invariant regardless.)
#[tokio::test]
async fn reject_writes_no_response_to_the_guest() {
    let ca = ca();
    let mint = Arc::new(CertMint::new(ca.clone()));
    let server_cfg = build_server_config(mint);
    let client_cfg = build_client_config();

    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream_addr = fake_upstream(captured.clone()).await;
    let (client_to_proxy, proxy_from_client) = tokio::io::duplex(64 * 1024);

    // Only GET /api/v2/logs* is allowed; the guest attempts a DELETE (the
    // campaign's stale-token branch-delete shape) — matches no policy → rejected.
    let inj = inject_entry("dd-secret-xyz", &["GET"], &["/api/v2/logs*"]);
    let resolver = Arc::new(StaticResolver::new().with("fake-upstream", upstream_addr));
    let proxy_task = tokio::spawn(async move {
        let injects: Vec<&InjectEntry> = vec![&inj];
        intercept::run(
            proxy_from_client,
            Vec::new(),
            resolver,
            intercept::StreamContext {
                sni: "fake-upstream",
                port: upstream_addr.port(),
                secrets: &[],
                injects: &injects,
                observes: &[],
                foreign_placeholders: &[],
                session_id: SessionId::new(),
                sink: None,
                refresher: None,
            },
            server_cfg,
            client_cfg,
        )
        .await
    });

    let mut tls_client = tls_client_to(&ca, client_to_proxy).await;
    tls_client
        .write_all(
            b"DELETE /repos/o/r/git/refs/heads/x HTTP/1.1\r\nHost: fake-upstream\r\nContent-Length: 0\r\n\r\n",
        )
        .await
        .unwrap();
    tls_client.flush().await.unwrap();

    // Read whatever the guest gets before the connection closes.
    let mut resp = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tls_client.read_to_end(&mut resp),
    )
    .await;

    let outcome = proxy_task.await.unwrap();
    assert!(
        matches!(outcome, Err(InterceptError::RequestRejected { .. })),
        "expected RequestRejected, got: {outcome:?}",
    );
    assert!(
        resp.is_empty(),
        "reject must write NO response to the guest (no synthesized 2xx/204); got: {:?}",
        String::from_utf8_lossy(&resp),
    );
    assert!(
        captured.lock().is_empty(),
        "upstream must see nothing on reject"
    );
}

// WS4: a minted inject entry within 5 min of expiry is re-minted via the
// refresher BEFORE injection, so the upstream sees the FRESH token — not the
// stale boot-time one. This is the end-to-end proof of the reads-401 fix.
struct FreshRefresher;

#[async_trait::async_trait]
impl InjectRefresher for FreshRefresher {
    async fn refresh(
        &self,
        _s: SessionId,
        _source: &engram_core::types::integration::CredentialMintSource,
    ) -> Option<RefreshedInject> {
        Some(RefreshedInject {
            secret: "fresh-token".into(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
        })
    }
}

#[tokio::test]
async fn near_expiry_inject_is_reminted_before_forwarding() {
    let ca = ca();
    let mint = Arc::new(CertMint::new(ca.clone()));
    let server_cfg = build_server_config(mint);
    let client_cfg = build_client_config();

    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream_addr = fake_upstream(captured.clone()).await;
    let (client_to_proxy, proxy_from_client) = tokio::io::duplex(64 * 1024);

    // A minted github inject whose token expires in 2 min — inside the 5-min
    // refresh window, so `intercept::run` re-mints it via the refresher first.
    let inj = InjectEntry {
        header_name: "Authorization".into(),
        header_template: "Bearer {}".into(),
        allow: HostList::from_manifest(&["fake-upstream".into()], &[]).unwrap(),
        policy: RequestPolicy {
            methods: vec!["GET".into()],
            path_globs: vec!["/user".into()],
            graphql: None,
        },
        mint_source: Some(
            engram_core::types::integration::CredentialMintSource::Connection {
                connection_id: "github-default".into(),
                provider: "github".into(),
            },
        ),
        cred: RefreshableCred::new(
            "stale-token".into(),
            Some(chrono::Utc::now() + chrono::Duration::minutes(2)),
        ),
    };
    let refresher: Arc<dyn InjectRefresher> = Arc::new(FreshRefresher);
    let resolver = Arc::new(StaticResolver::new().with("fake-upstream", upstream_addr));
    let proxy_task = tokio::spawn(async move {
        let injects: Vec<&InjectEntry> = vec![&inj];
        intercept::run(
            proxy_from_client,
            Vec::new(),
            resolver,
            intercept::StreamContext {
                sni: "fake-upstream",
                port: upstream_addr.port(),
                secrets: &[],
                injects: &injects,
                observes: &[],
                foreign_placeholders: &[],
                session_id: SessionId::new(),
                sink: None,
                refresher: Some(refresher.as_ref()),
            },
            server_cfg,
            client_cfg,
        )
        .await
    });

    let mut tls_client = tls_client_to(&ca, client_to_proxy).await;
    tls_client
        .write_all(b"GET /user HTTP/1.1\r\nHost: fake-upstream\r\nContent-Length: 0\r\n\r\n")
        .await
        .unwrap();
    tls_client.flush().await.unwrap();
    let mut buf = [0u8; 1024];
    let _ = tls_client.read(&mut buf).await;
    // Close the client half so the proxy's `copy_bidirectional` sees EOF and the
    // task completes (same choreography the substitution test uses).
    let _ = tls_client.shutdown().await;
    drop(tls_client);

    let _ = proxy_task.await.unwrap();
    let seen = String::from_utf8_lossy(&captured.lock()).to_string();
    assert!(
        seen.contains("Authorization: Bearer fresh-token"),
        "upstream must see the RE-MINTED token, got head: {seen:?}",
    );
    assert!(
        !seen.contains("stale-token"),
        "the stale boot-time token must never reach upstream",
    );
}

/// A TLS upstream that captures the request head, then replies with the given
/// response bytes and closes. Used by the observe tests to return a JSON body.
/// Read a whole HTTP/1 request: the header block, then the body its
/// `Content-Length` declares.
///
/// Stopping at the header terminator is what a fixture wants to do and what a
/// real server never does. It made the GraphQL tests flaky: those requests
/// carry a body, so when the head and the body landed in different TLS records
/// the upstream answered and closed the connection while the proxy was still
/// writing, and the proxy failed with `broken pipe`. It also made `captured`
/// non-deterministic — the body was present only when it rode the same record.
async fn read_http1_request<S>(tls: &mut S) -> Vec<u8>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let head_end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        match tls.read(&mut tmp).await {
            Ok(0) => return buf,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => return buf,
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
    let content_length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < head_end + content_length {
        match tls.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => break,
        }
    }
    buf
}

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
        let buf = read_http1_request(&mut tls).await;
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
            path_globs: vec!["/repos/*/issues".into()],
            graphql: None,
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
        url_fallback: None,
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
            resolver,
            intercept::StreamContext {
                sni: "fake-upstream",
                port: upstream_addr.port(),
                secrets: &[],
                injects: &[],
                observes: &observes,
                foreign_placeholders: &[],
                session_id,
                sink: Some(&sink),
                refresher: None, // WS4: no inject refresher in this test
            },
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
    outcome.expect("the proxy must complete a permitted request");

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
            path_globs: vec!["/repos/*/issues".into()],
            graphql: None,
        },
        provider: "github".into(),
        asset_kind: "issue".into(),
        surface: "asset".into(),
        success: SuccessRule::StatusClass2xx,
        data: vec![("number".into(), "$.resp.number".into())],
        fetchable: None,
        url_fallback: None,
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
            resolver,
            intercept::StreamContext {
                sni: "fake-upstream",
                port: upstream_addr.port(),
                secrets: &[],
                injects: &[],
                observes: &observes,
                foreign_placeholders: &[],
                session_id: SessionId::new(),
                sink: Some(&sink),
                refresher: None, // WS4: no inject refresher in this test
            },
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

// ---------------------------------------------------------------------------
// ADR 0059: GraphQL operation gating + observation.
// ---------------------------------------------------------------------------

/// A GraphQL inject entry gating `POST /graphql` for one (operation, field),
/// injecting `Authorization: Bearer <secret>` (mirrors GitHub's mint plane).
fn graphql_inject_entry(secret: &str, op: GraphqlOperation, field: &str) -> InjectEntry {
    InjectEntry {
        header_name: "Authorization".into(),
        header_template: "Bearer {}".into(),
        allow: HostList::from_manifest(&["fake-upstream".into()], &[]).unwrap(),
        policy: RequestPolicy {
            methods: vec!["POST".into()],
            path_globs: vec!["/graphql".into()],
            graphql: Some(GraphqlMatch {
                operation: op,
                field: field.into(),
            }),
        },
        mint_source: None,
        cred: engram_egress_proxy::RefreshableCred::new(secret.into(), None),
    }
}

/// A GraphQL observe entry for one (operation, field), emitting an `issue` asset
/// from `$.resp.data.createIssue.issue.id`, gated by `NoGraphqlErrors`.
fn graphql_observe_entry(op: GraphqlOperation, field: &str) -> ObserveEntry {
    ObserveEntry {
        allow: HostList::from_manifest(&["fake-upstream".into()], &[]).unwrap(),
        policy: RequestPolicy {
            methods: vec!["POST".into()],
            path_globs: vec!["/graphql".into()],
            graphql: Some(GraphqlMatch {
                operation: op,
                field: field.into(),
            }),
        },
        provider: "github".into(),
        asset_kind: "issue".into(),
        surface: "asset".into(),
        success: SuccessRule::NoGraphqlErrors,
        data: vec![("id".into(), "$.resp.data.createIssue.issue.id".into())],
        fetchable: None,
        url_fallback: None,
    }
}

/// Build a `POST /graphql` request whose body is the JSON envelope `{"query": …}`.
fn graphql_post(body_json: &str) -> Vec<u8> {
    format!(
        "POST /graphql HTTP/1.1\r\nHost: fake-upstream\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body_json.len(),
        body_json,
    )
    .into_bytes()
}

/// A `{"query": …}` JSON envelope around a raw GraphQL document.
fn gql(query: &str) -> String {
    serde_json::json!({ "query": query }).to_string()
}

/// Run an intercept with the given GraphQL inject entries against a single client
/// request; return (proxy outcome, bytes the upstream received). Upstream replies
/// `200 OK` (the gating tests only care whether the request was forwarded).
async fn run_graphql_inject(
    ca: Arc<Ca>,
    injects: Vec<InjectEntry>,
    request: Vec<u8>,
) -> (Result<(), InterceptError>, Vec<u8>) {
    let mint = Arc::new(CertMint::new(ca.clone()));
    let server_cfg = build_server_config(mint);
    let client_cfg = build_client_config();
    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream_addr = fake_upstream(captured.clone()).await;
    let (client_to_proxy, proxy_from_client) = tokio::io::duplex(64 * 1024);
    let resolver = Arc::new(StaticResolver::new().with("fake-upstream", upstream_addr));
    let proxy_task = tokio::spawn(async move {
        let inj_refs: Vec<&InjectEntry> = injects.iter().collect();
        intercept::run(
            proxy_from_client,
            Vec::new(),
            resolver,
            intercept::StreamContext {
                sni: "fake-upstream",
                port: upstream_addr.port(),
                secrets: &[],
                injects: &inj_refs,
                observes: &[],
                foreign_placeholders: &[],
                session_id: SessionId::new(),
                sink: None,
                refresher: None, // WS4: no inject refresher in this test
            },
            server_cfg,
            client_cfg,
        )
        .await
    });

    let mut tls_client = tls_client_to(&ca, client_to_proxy).await;
    tls_client.write_all(&request).await.unwrap();
    tls_client.flush().await.unwrap();
    // Read to EOF rather than taking one read and dropping the socket. The
    // proxy forces `Connection: close`, so EOF is the end of the exchange —
    // and tearing the client down before the proxy finished writing gave the
    // proxy a `broken pipe`, which is what the "tolerated error" lists in these
    // tests were really hiding.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut sink = Vec::new();
        let _ = tls_client.read_to_end(&mut sink).await;
    })
    .await;
    drop(tls_client);

    let outcome = proxy_task.await.unwrap();
    let cap = captured.lock().clone();
    (outcome, cap)
}

#[tokio::test]
async fn graphql_allows_mapped_mutation() {
    let ca = ca();
    let inj = graphql_inject_entry("tok-abc", GraphqlOperation::Mutation, "mergePullRequest");
    let req = graphql_post(&gql(
        "mutation { mergePullRequest(input: {}) { clientMutationId } }",
    ));
    let (outcome, captured) = run_graphql_inject(ca, vec![inj], req).await;
    outcome.expect("the proxy must complete a permitted request");
    let seen = String::from_utf8_lossy(&captured);
    assert!(
        seen.contains("Authorization: Bearer tok-abc\r\n"),
        "upstream should see the injected token on the mapped mutation; got: {seen}",
    );
    assert!(seen.starts_with("POST /graphql HTTP/1.1\r\n"));
}

#[tokio::test]
async fn graphql_denies_unmapped_mutation() {
    let ca = ca();
    // Granted: mergePullRequest. The client asks for a different mutation.
    let inj = graphql_inject_entry("tok-abc", GraphqlOperation::Mutation, "mergePullRequest");
    let req = graphql_post(&gql(
        "mutation { deleteRepository(input: {}) { clientMutationId } }",
    ));
    let (outcome, captured) = run_graphql_inject(ca, vec![inj], req).await;
    assert!(
        matches!(outcome, Err(InterceptError::GraphqlRejected { .. })),
        "expected GraphqlRejected, got: {outcome:?}",
    );
    assert!(
        captured.is_empty(),
        "upstream must see nothing when the operation is not permitted",
    );
}

#[tokio::test]
async fn graphql_denies_when_one_of_multiple_fields_unmapped() {
    let ca = ca();
    // Granted: query viewer only. The doc selects viewer AND repository — set
    // coverage requires BOTH, so the whole request is denied.
    let inj = graphql_inject_entry("tok-abc", GraphqlOperation::Query, "viewer");
    let req = graphql_post(&gql(
        "query { viewer { login } repository(owner: \"o\", name: \"r\") { id } }",
    ));
    let (outcome, captured) = run_graphql_inject(ca, vec![inj], req).await;
    assert!(
        matches!(outcome, Err(InterceptError::GraphqlRejected { .. })),
        "expected GraphqlRejected (uncovered field), got: {outcome:?}",
    );
    assert!(captured.is_empty());
}

#[tokio::test]
async fn graphql_multi_field_query_injects_one_authorization_header() {
    let ca = ca();
    let viewer = graphql_inject_entry("tok-abc", GraphqlOperation::Query, "viewer");
    let repository = graphql_inject_entry("tok-abc", GraphqlOperation::Query, "repository");
    let req = graphql_post(&gql(
        "query { viewer { login } repository(owner: \"o\", name: \"r\") { id } }",
    ));
    let (outcome, captured) = run_graphql_inject(ca, vec![viewer, repository], req).await;
    outcome.expect("the proxy must complete a permitted request");
    let seen = String::from_utf8_lossy(&captured);
    assert_eq!(
        seen.to_ascii_lowercase().matches("authorization:").count(),
        1,
        "multi-field GraphQL requests must emit one credential header; got: {seen}",
    );
    assert!(seen.contains("Authorization: Bearer tok-abc\r\n"));
}

#[tokio::test]
async fn graphql_allows_aliased_field() {
    let ca = ca();
    // An alias must resolve to the underlying field — `a:` must not bypass the gate.
    let inj = graphql_inject_entry("tok-abc", GraphqlOperation::Mutation, "mergePullRequest");
    let req = graphql_post(&gql(
        "mutation { a: mergePullRequest(input: {}) { clientMutationId } }",
    ));
    let (outcome, captured) = run_graphql_inject(ca, vec![inj], req).await;
    outcome.expect("the proxy must complete a permitted request");
    assert!(String::from_utf8_lossy(&captured).contains("Authorization: Bearer tok-abc\r\n"));
}

// `gh`'s schema feature detection (run before `gh pr checks` / `gh pr create`)
// sends aliased introspection queries like
// `query PullRequest_fields{PullRequest: __type(name: "PullRequest"){...}}`.
// A `__type` inject entry must cover them — including the aliased,
// multi-field shape — or those commands die on the probe.
#[tokio::test]
async fn graphql_allows_aliased_type_introspection() {
    let ca = ca();
    let inj = graphql_inject_entry("tok-abc", GraphqlOperation::Query, "__type");
    let req = graphql_post(&gql(
        "query PullRequest_fields{PullRequest: __type(name: \"PullRequest\"){fields(includeDeprecated: true){name}},StatusCheckRollupContextConnection: __type(name: \"StatusCheckRollupContextConnection\"){fields(includeDeprecated: true){name}}}",
    ));
    let (outcome, captured) = run_graphql_inject(ca, vec![inj], req).await;
    outcome.expect("the proxy must complete a permitted request");
    let seen = String::from_utf8_lossy(&captured);
    assert!(seen.contains("Authorization: Bearer tok-abc\r\n"));
}

#[tokio::test]
async fn graphql_allows_mapped_query_and_anonymous_shorthand() {
    let ca = ca();
    let inj = graphql_inject_entry("tok-q", GraphqlOperation::Query, "viewer");
    for query in ["query { viewer { login } }", "{ viewer { login } }"] {
        let req = graphql_post(&gql(query));
        let (outcome, captured) = run_graphql_inject(ca.clone(), vec![inj.clone()], req).await;
        if let Err(e) = &outcome {
            let msg = format!("{e}");
            if !msg.contains("close_notify") && !msg.contains("UnexpectedEof") {
                panic!("proxy returned unexpected error on `{query}`: {e}");
            }
        }
        assert!(
            String::from_utf8_lossy(&captured).contains("Authorization: Bearer tok-q\r\n"),
            "query `{query}` should be allowed",
        );
    }
}

#[tokio::test]
async fn graphql_denies_oversized_body() {
    let ca = ca();
    let inj = graphql_inject_entry("tok-abc", GraphqlOperation::Mutation, "mergePullRequest");
    // Declare a Content-Length far over the 256 KiB cap — rejected before reading.
    let body = gql("mutation { mergePullRequest(input: {}) { clientMutationId } }");
    let req = format!(
        "POST /graphql HTTP/1.1\r\nHost: fake-upstream\r\nContent-Length: 9999999\r\n\r\n{body}",
    )
    .into_bytes();
    let (outcome, captured) = run_graphql_inject(ca, vec![inj], req).await;
    assert!(
        matches!(outcome, Err(InterceptError::GraphqlRejected { .. })),
        "expected GraphqlRejected (over cap), got: {outcome:?}",
    );
    assert!(captured.is_empty());
}

#[tokio::test]
async fn graphql_denies_unparseable_body() {
    let ca = ca();
    let inj = graphql_inject_entry("tok-abc", GraphqlOperation::Mutation, "mergePullRequest");
    // A syntactically broken GraphQL document — fail closed.
    let req = graphql_post(r#"{"query":"mutation { "}"#);
    let (outcome, captured) = run_graphql_inject(ca, vec![inj], req).await;
    assert!(
        matches!(outcome, Err(InterceptError::GraphqlRejected { .. })),
        "expected GraphqlRejected (unparseable), got: {outcome:?}",
    );
    assert!(captured.is_empty());
}

#[tokio::test]
async fn graphql_denies_batched_array() {
    let ca = ca();
    let inj = graphql_inject_entry("tok-q", GraphqlOperation::Query, "viewer");
    // A batched (array) request is unsupported — deny.
    let req = graphql_post(r#"[{"query":"query { viewer { login } }"}]"#);
    let (outcome, captured) = run_graphql_inject(ca, vec![inj], req).await;
    assert!(
        matches!(outcome, Err(InterceptError::GraphqlRejected { .. })),
        "expected GraphqlRejected (batched array), got: {outcome:?}",
    );
    assert!(captured.is_empty());
}

/// Run an intercept with a GraphQL observe entry, returning the assets the sink
/// collected. `upstream_response` is the raw HTTP response bytes the fake upstream
/// returns.
async fn run_graphql_observe(
    ca: Arc<Ca>,
    observe: ObserveEntry,
    request: Vec<u8>,
    upstream_response: Vec<u8>,
) -> Vec<ObservedAsset> {
    let mint = Arc::new(CertMint::new(ca.clone()));
    let server_cfg = build_server_config(mint);
    let client_cfg = build_client_config();
    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let upstream_addr = fake_upstream_resp(captured.clone(), upstream_response).await;
    let (client_to_proxy, proxy_from_client) = tokio::io::duplex(64 * 1024);

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
            resolver,
            intercept::StreamContext {
                sni: "fake-upstream",
                port: upstream_addr.port(),
                secrets: &[],
                injects: &[],
                observes: &observes,
                foreign_placeholders: &[],
                session_id: SessionId::new(),
                sink: Some(&sink),
                refresher: None, // WS4: no inject refresher in this test
            },
            server_cfg,
            client_cfg,
        )
        .await
    });

    let mut tls_client = tls_client_to(&ca, client_to_proxy).await;
    tls_client.write_all(&request).await.unwrap();
    tls_client.flush().await.unwrap();
    let mut resp = Vec::new();
    let _ = tls_client.read_to_end(&mut resp).await;
    let _ = tls_client.shutdown().await;
    drop(tls_client);
    let _ = proxy_task.await.unwrap();

    let assets: Vec<ObservedAsset> = collected.lock().iter().map(|(_, a)| a.clone()).collect();
    assets
}

#[tokio::test]
async fn graphql_observe_emits_on_no_errors() {
    let ca = ca();
    let observe = graphql_observe_entry(GraphqlOperation::Mutation, "createIssue");
    let req = graphql_post(&gql("mutation { createIssue(input: {}) { issue { id } } }"));
    let body = r#"{"data":{"createIssue":{"issue":{"id":"I_1"}}}}"#;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes();
    let assets = run_graphql_observe(ca, observe, req, response).await;
    assert_eq!(assets.len(), 1, "exactly one asset emitted");
    assert_eq!(assets[0].provider, "github");
    assert_eq!(assets[0].asset_kind, "issue");
    assert_eq!(
        assets[0].data.get("id"),
        Some(&serde_json::json!("I_1")),
        "asset id extracted from $.resp.data.createIssue.issue.id",
    );
}

// The `gh pr create` regression: its createPullRequest mutation selects only
// `pullRequest { id url }`, so the response body carries none of the card
// fields. Parity with the REST asset comes from the request VARIABLES
// (`$.vars.input.*` → title/branches) + the URL fallback (repo/number derived
// from the returned PR URL) — end to end through the real intercept path.
#[tokio::test]
async fn graphql_observe_reaches_rest_parity_for_gh_pr_create() {
    let ca = ca();
    let observe = ObserveEntry {
        allow: HostList::from_manifest(&["fake-upstream".into()], &[]).unwrap(),
        policy: RequestPolicy {
            methods: vec!["POST".into()],
            path_globs: vec!["/graphql".into()],
            graphql: Some(GraphqlMatch {
                operation: GraphqlOperation::Mutation,
                field: "createPullRequest".into(),
            }),
        },
        provider: "github".into(),
        asset_kind: "pull_request".into(),
        surface: "asset".into(),
        success: SuccessRule::NoGraphqlErrors,
        data: vec![
            (
                "number".into(),
                "$.resp.data.createPullRequest.pullRequest.number".into(),
            ),
            ("title".into(), "$.vars.input.title".into()),
            (
                "repo".into(),
                "$.resp.data.createPullRequest.pullRequest.repository.nameWithOwner".into(),
            ),
            ("head_branch".into(), "$.vars.input.headRefName".into()),
            ("base_branch".into(), "$.vars.input.baseRefName".into()),
        ],
        fetchable: Some("$.resp.data.createPullRequest.pullRequest.url".into()),
        url_fallback: Some(engram_egress_proxy::UrlFallback {
            pattern: "https://github.com/{owner}/{name}/pull/{number:int}".into(),
            fields: vec![
                ("repo".into(), "{owner}/{name}".into()),
                ("number".into(), "{number}".into()),
            ],
        }),
    };
    // The exact envelope `gh` sends: named single mutation + variables.
    let envelope = serde_json::json!({
        "query": "mutation PullRequestCreate($input: CreatePullRequestInput!) { createPullRequest(input: $input) { pullRequest { id url } } }",
        "variables": {"input": {
            "repositoryId": "R_1",
            "title": "Fix the flux capacitor",
            "headRefName": "fix-flux",
            "baseRefName": "main",
        }},
    })
    .to_string();
    let req = graphql_post(&envelope);
    let body = r#"{"data":{"createPullRequest":{"pullRequest":{"id":"PR_1","url":"https://github.com/octo/engrams/pull/97"}}}}"#;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes();
    let assets = run_graphql_observe(ca, observe, req, response).await;
    assert_eq!(assets.len(), 1, "exactly one asset emitted");
    let a = &assets[0];
    assert_eq!(a.asset_kind, "pull_request");
    assert_eq!(
        a.data.get("title"),
        Some(&serde_json::json!("Fix the flux capacitor"))
    );
    assert_eq!(
        a.data.get("head_branch"),
        Some(&serde_json::json!("fix-flux"))
    );
    assert_eq!(a.data.get("base_branch"), Some(&serde_json::json!("main")));
    assert_eq!(a.data.get("repo"), Some(&serde_json::json!("octo/engrams")));
    assert_eq!(a.data.get("number"), Some(&serde_json::json!(97)));
    assert_eq!(
        a.fetchable_url.as_deref(),
        Some("https://github.com/octo/engrams/pull/97")
    );
}

#[tokio::test]
async fn graphql_observe_suppressed_on_errors() {
    let ca = ca();
    let observe = graphql_observe_entry(GraphqlOperation::Mutation, "createIssue");
    let req = graphql_post(&gql("mutation { createIssue(input: {}) { issue { id } } }"));
    // HTTP 200 but a GraphQL-level error → no side effect → no asset (the key
    // difference from REST, where 2xx alone would emit).
    let body = r#"{"errors":[{"message":"nope"}],"data":null}"#;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )
    .into_bytes();
    let assets = run_graphql_observe(ca, observe, req, response).await;
    assert!(
        assets.is_empty(),
        "a GraphQL `errors` response must not emit an asset; got: {assets:?}",
    );
}

// ── HTTP/2 through the full intercept path ────────────────────────────────
//
// Every earlier HTTP/2 test drove `run_h2` directly on two cleartext duplex
// pipes, so no test had ever taken the HTTP/2 leg through `intercept::run` —
// the ALPN mirroring, the guest-side leaf mint, and the upstream TLS
// handshake were all unexercised for h2. That is the leg a real Google gRPC
// call takes.

/// A TLS upstream that negotiates h2 and answers one gRPC-shaped stream. It
/// echoes the credential it received in a header, a DATA frame and a trailer,
/// so the test can prove all three are redacted on the way back.
async fn h2_upstream(seen_authorization: Arc<Mutex<Option<String>>>) -> SocketAddr {
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
    let mut cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .unwrap();
    cfg.alpn_protocols = vec![b"h2".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(cfg));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let tls = acceptor.accept(stream).await.unwrap();
        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
        let mut server = h2::server::handshake(tls).await.unwrap();
        while let Some(Ok((request, mut respond))) = server.accept().await {
            // Handle the stream in its own task. `server.accept()` is what
            // drives this connection's I/O, so awaiting the request body inline
            // stops the connection from ever reading the DATA frames that body
            // is waiting for. It only appeared to work because the DATA usually
            // rides the same TCP segment as the HEADERS and is already buffered
            // when `accept()` returns; when it arrived in a later segment the
            // exchange hung.
            let seen = seen_authorization.clone();
            tokio::spawn(async move {
                *seen.lock() = request
                    .headers()
                    .get("authorization")
                    .map(|value| value.to_str().unwrap().to_string());
                let mut body = request.into_body();
                while let Some(Ok(data)) = body.data().await {
                    body.flow_control().release_capacity(data.len()).unwrap();
                }
                let response = http::Response::builder()
                    .status(200)
                    .header("content-type", "application/grpc")
                    .header("x-reflected-token", "dd-secret-xyz")
                    .body(())
                    .unwrap();
                let mut out = respond.send_response(response, false).unwrap();
                out.send_data(bytes::Bytes::from_static(b"token=dd-secret-xyz"), false)
                    .unwrap();
                let mut trailers = http::HeaderMap::new();
                trailers.insert("grpc-status", http::HeaderValue::from_static("0"));
                trailers.insert(
                    "x-trailer-token",
                    http::HeaderValue::from_static("dd-secret-xyz"),
                );
                out.send_trailers(trailers).unwrap();
            });
        }
    });
    addr
}

#[tokio::test]
async fn h2_intercept_injects_and_redacts_across_headers_body_and_trailers() {
    let ca = ca();
    let mint = Arc::new(CertMint::new(ca.clone()));
    let server_cfg = build_server_config(mint);
    let client_cfg = build_client_config();

    let seen_authorization: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let upstream_addr = h2_upstream(seen_authorization.clone()).await;
    let (client_to_proxy, proxy_from_client) = tokio::io::duplex(64 * 1024);

    let inject = InjectEntry {
        header_name: "authorization".into(),
        header_template: "Bearer {}".into(),
        allow: HostList::from_manifest(&["fake-upstream".into()], &[]).unwrap(),
        policy: RequestPolicy {
            methods: vec!["POST".into()],
            path_globs: vec!["/engrams.test.v1.Echo/*".into()],
            graphql: None,
        },
        mint_source: None,
        cred: RefreshableCred::new("dd-secret-xyz".into(), None),
    };
    let resolver = Arc::new(StaticResolver::new().with("fake-upstream", upstream_addr));
    let proxy_task = tokio::spawn(async move {
        let injects: Vec<&InjectEntry> = vec![&inject];
        intercept::run(
            proxy_from_client,
            Vec::new(),
            resolver,
            intercept::StreamContext {
                sni: "fake-upstream",
                port: upstream_addr.port(),
                secrets: &[],
                injects: &injects,
                observes: &[],
                foreign_placeholders: &[],
                session_id: SessionId::new(),
                sink: None,
                refresher: None,
            },
            server_cfg,
            client_cfg,
        )
        .await
    });

    // The guest offers h2 only, so the proxy must mirror h2 on both legs.
    let pem = ca.cert_pem.clone();
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(
            rustls_pemfile::certs(&mut pem.as_bytes())
                .next()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
    let mut cli_cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cli_cfg.alpn_protocols = vec![b"h2".to_vec()];
    let connector = TlsConnector::from(Arc::new(cli_cfg));
    let server_name: rustls::pki_types::ServerName<'static> = "fake-upstream".try_into().unwrap();
    let tls = connector
        .connect(server_name, client_to_proxy)
        .await
        .unwrap();
    assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));

    let (mut guest, connection) = h2::client::handshake(tls).await.unwrap();
    let driver = tokio::spawn(connection);
    guest = guest.ready().await.unwrap();
    let request = http::Request::builder()
        .method("POST")
        .uri("https://fake-upstream/engrams.test.v1.Echo/Say")
        .header("content-type", "application/grpc")
        .body(())
        .unwrap();
    let (response, mut request_body) = guest.send_request(request, false).unwrap();
    request_body
        .send_data(bytes::Bytes::from_static(b"ping"), true)
        .unwrap();
    let response = tokio::time::timeout(std::time::Duration::from_secs(30), response)
        .await
        .expect("the h2 leg must complete")
        .unwrap();

    assert_eq!(response.status(), 200);
    // The credential the proxy attached never came from the guest.
    assert_eq!(
        seen_authorization.lock().clone(),
        Some("Bearer dd-secret-xyz".to_string()),
    );
    // ...and none of the three places the upstream reflected it returns it.
    assert_eq!(
        response.headers().get("x-reflected-token").unwrap(),
        "*************",
    );
    let mut body = response.into_body();
    let mut received = Vec::new();
    while let Some(Ok(data)) = body.data().await {
        body.flow_control().release_capacity(data.len()).unwrap();
        received.extend_from_slice(&data);
    }
    assert_eq!(received, b"token=*************");
    let trailers = body.trailers().await.unwrap().unwrap();
    assert_eq!(trailers.get("grpc-status").unwrap(), "0");
    assert_eq!(trailers.get("x-trailer-token").unwrap(), "*************");

    drop(guest);
    driver.abort();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), proxy_task).await;
}

#[tokio::test]
async fn h2_intercept_denies_a_grpc_credential_method() {
    // ADR 0109 S3: the gRPC form of service-account key creation. The REST
    // shapes the old denylist matched could never see this path.
    let ca = ca();
    let mint = Arc::new(CertMint::new(ca.clone()));
    let server_cfg = build_server_config(mint);
    let client_cfg = build_client_config();

    let seen_authorization: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let upstream_addr = h2_upstream(seen_authorization.clone()).await;
    let (client_to_proxy, proxy_from_client) = tokio::io::duplex(64 * 1024);

    let inject = InjectEntry {
        header_name: "authorization".into(),
        header_template: "Bearer {}".into(),
        allow: HostList::from_manifest(&["iam.googleapis.com".into()], &[]).unwrap(),
        policy: RequestPolicy::default(), // deliberately broad
        mint_source: None,
        cred: RefreshableCred::new("google-access-token".into(), None),
    };
    let resolver = Arc::new(StaticResolver::new().with("iam.googleapis.com", upstream_addr));
    let proxy_task = tokio::spawn(async move {
        let injects: Vec<&InjectEntry> = vec![&inject];
        intercept::run(
            proxy_from_client,
            Vec::new(),
            resolver,
            intercept::StreamContext {
                sni: "iam.googleapis.com",
                port: upstream_addr.port(),
                secrets: &[],
                injects: &injects,
                observes: &[],
                foreign_placeholders: &[],
                session_id: SessionId::new(),
                sink: None,
                refresher: None,
            },
            server_cfg,
            client_cfg,
        )
        .await
    });

    let pem = ca.cert_pem.clone();
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(
            rustls_pemfile::certs(&mut pem.as_bytes())
                .next()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
    let mut cli_cfg = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    cli_cfg.alpn_protocols = vec![b"h2".to_vec()];
    let connector = TlsConnector::from(Arc::new(cli_cfg));
    let server_name: rustls::pki_types::ServerName<'static> =
        "iam.googleapis.com".try_into().unwrap();
    let tls = connector
        .connect(server_name, client_to_proxy)
        .await
        .unwrap();

    let (mut guest, connection) = h2::client::handshake(tls).await.unwrap();
    let driver = tokio::spawn(connection);
    guest = guest.ready().await.unwrap();
    let request = http::Request::builder()
        .method("POST")
        .uri("https://iam.googleapis.com/google.iam.admin.v1.IAM/CreateServiceAccountKey")
        .header("content-type", "application/grpc")
        .body(())
        .unwrap();
    let (response, _) = guest.send_request(request, true).unwrap();
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), response)
        .await
        .expect("the denial must be prompt")
        .unwrap();

    assert_eq!(response.status(), 403);
    // The request never reached the upstream, so no credential was spent.
    assert!(seen_authorization.lock().is_none());

    drop(guest);
    driver.abort();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), proxy_task).await;
}
