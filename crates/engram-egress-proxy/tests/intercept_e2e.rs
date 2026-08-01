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
    self, build_client_config, build_server_config, InterceptError,
};
use engram_egress_proxy::observe::{ObserveSink, ObservedAsset};
use engram_egress_proxy::policy::HostList;
use engram_egress_proxy::registry::{
    GraphqlMatch, GraphqlOperation, InjectEntry, InjectRefresher, ObserveEntry, RefreshableCred,
    RefreshedInject, RequestPolicy, SecretEntry, SuccessRule,
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
            None, // WS4: no inject refresher in this test
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
            None, // WS4: no inject refresher in this test
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
            None, // WS4: no inject refresher in this test
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
            "fake-upstream",
            upstream_addr.port(),
            resolver,
            &[],
            &injects,
            &[],
            SessionId::new(),
            None,
            None,
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
            "fake-upstream",
            upstream_addr.port(),
            resolver,
            &[],
            &injects,
            &[],
            SessionId::new(),
            None,
            None,
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
            "fake-upstream",
            upstream_addr.port(),
            resolver,
            &[],
            &injects,
            &[],
            SessionId::new(),
            None,
            None, // WS4: no inject refresher in this test
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
            "fake-upstream",
            upstream_addr.port(),
            resolver,
            &[],
            &injects,
            &[],
            SessionId::new(),
            None,
            None,
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
            "fake-upstream",
            upstream_addr.port(),
            resolver,
            &[],
            &injects,
            &[],
            SessionId::new(),
            None,
            Some(refresher.as_ref()),
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
            "fake-upstream",
            upstream_addr.port(),
            resolver,
            &[],
            &[],
            &observes,
            session_id,
            Some(&sink),
            None, // WS4: no inject refresher in this test
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
            "fake-upstream",
            upstream_addr.port(),
            resolver,
            &[],
            &[],
            &observes,
            SessionId::new(),
            Some(&sink),
            None, // WS4: no inject refresher in this test
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
            "fake-upstream",
            upstream_addr.port(),
            resolver,
            &[],
            &inj_refs,
            &[],
            SessionId::new(),
            None,
            None, // WS4: no inject refresher in this test
            server_cfg,
            client_cfg,
        )
        .await
    });

    let mut tls_client = tls_client_to(&ca, client_to_proxy).await;
    tls_client.write_all(&request).await.unwrap();
    tls_client.flush().await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut buf = [0u8; 1024];
        let _ = tls_client.read(&mut buf).await;
    })
    .await;
    let _ = tls_client.shutdown().await;
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
    if let Err(e) = &outcome {
        let msg = format!("{e}");
        if !msg.contains("close_notify") && !msg.contains("UnexpectedEof") {
            panic!("proxy returned unexpected error: {e}");
        }
    }
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
    if let Err(e) = &outcome {
        let msg = format!("{e}");
        if !msg.contains("close_notify") && !msg.contains("UnexpectedEof") {
            panic!("proxy returned unexpected error: {e}");
        }
    }
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
    if let Err(e) = &outcome {
        let msg = format!("{e}");
        if !msg.contains("close_notify") && !msg.contains("UnexpectedEof") {
            panic!("proxy returned unexpected error: {e}");
        }
    }
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
    if let Err(e) = &outcome {
        let msg = format!("{e}");
        if !msg.contains("close_notify") && !msg.contains("UnexpectedEof") {
            panic!("proxy returned unexpected error: {e}");
        }
    }
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
            "fake-upstream",
            upstream_addr.port(),
            resolver,
            &[],
            &[],
            &observes,
            SessionId::new(),
            Some(&sink),
            None, // WS4: no inject refresher in this test
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
