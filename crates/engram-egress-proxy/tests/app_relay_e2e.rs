//! ADR 0118: the same-session app-to-app short circuit, end to end.
//!
//! Drives the whole new code path with real TLS on the guest side and a real
//! socket on the sibling side:
//!
//!   guest TLS client → app_relay::serve → (dialer) → sibling's "guest" port
//!
//! What this proves that the unit tests cannot: the proxy presents a leaf the
//! guest's CA validates for the APP's hostname, and the plaintext behind that
//! TLS session reaches the sibling port unaltered in both directions.
//!
//! The vsock leg beneath the dialer is not re-proved here — `proxy_port_loopback`
//! (Firecracker, in CI) already covers it, and re-proving it would mean booting
//! a VM to test a wrapper. This test uses a loopback listener as the sibling,
//! which is exactly what the dialer's `TunnelStream` abstracts.

use std::sync::Arc;

use async_trait::async_trait;
use engram_core::SandboxId;
use engram_egress_proxy::{app_relay, Ca, CertMint, GuestPortDialer, TunnelStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;

fn ca() -> Arc<Ca> {
    let tmp = tempfile::tempdir().unwrap().keep();
    Arc::new(Ca::load_or_generate(&tmp).unwrap())
}

/// A dialer whose "guest port" is an ordinary loopback listener. Stands in for
/// the host-agent's vsock-relay dialer; the contract is identical (hand back a
/// byte stream already connected to the sibling).
struct LoopbackDialer {
    addr: std::net::SocketAddr,
}

#[async_trait]
impl GuestPortDialer for LoopbackDialer {
    async fn dial(
        &self,
        _sandbox: SandboxId,
        _port: u16,
    ) -> std::io::Result<Box<dyn TunnelStream>> {
        Ok(Box::new(TcpStream::connect(self.addr).await?))
    }
}

/// A dialer that always refuses, standing in for a sibling whose port is dead.
struct RefusingDialer;

#[async_trait]
impl GuestPortDialer for RefusingDialer {
    async fn dial(
        &self,
        _sandbox: SandboxId,
        _port: u16,
    ) -> std::io::Result<Box<dyn TunnelStream>> {
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "nothing listening",
        ))
    }
}

/// A TLS client that trusts the engram CA, as a guest does — the CA is staged
/// into every sandbox's trust store, which is what lets the proxy mint a leaf
/// the guest accepts for a hostname it does not actually own.
async fn tls_client_to(
    ca: &Ca,
    hostname: &'static str,
    stream: tokio::io::DuplexStream,
) -> std::io::Result<tokio_rustls::client::TlsStream<tokio::io::DuplexStream>> {
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
    let server_name: rustls::pki_types::ServerName<'static> = hostname.try_into().unwrap();
    connector.connect(server_name, stream).await
}

const APP_HOST: &str = "api-tidy-swift-otters.preview.example.com";

#[tokio::test]
async fn splices_a_tls_call_to_the_sibling_guest_port() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ca = ca();

    // The sibling app: an ordinary echo server standing in for a guest port.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 64];
        let n = sock.read(&mut buf).await.unwrap();
        // Echo back with a marker so the assertion cannot pass on a loopback
        // of the client's own bytes.
        sock.write_all(format!("sibling saw: {}", String::from_utf8_lossy(&buf[..n])).as_bytes())
            .await
            .unwrap();
        sock.flush().await.unwrap();
    });

    let (client_side, proxy_side) = tokio::io::duplex(64 * 1024);
    let server_cfg =
        engram_egress_proxy::intercept::build_server_config(Arc::new(CertMint::new(ca.clone())));
    let dialer = LoopbackDialer { addr };
    let served = tokio::spawn(async move {
        app_relay::serve(
            proxy_side,
            Vec::new(),
            SandboxId::new(),
            8080,
            &dialer,
            server_cfg,
        )
        .await
    });

    // The guest speaks TLS to the app's PUBLIC hostname and must get a leaf its
    // CA validates — the proxy is impersonating a name it does not own, which
    // only works because the guest trusts our CA.
    let mut tls = tls_client_to(&ca, APP_HOST, client_side)
        .await
        .expect("guest TLS handshake to its own app hostname");
    tls.write_all(b"hello sibling").await.unwrap();
    tls.flush().await.unwrap();
    // Half-close so the splice sees EOF on the guest→sibling direction. Without
    // it `copy_bidirectional` waits forever for a client that has stopped
    // talking but not hung up.
    tls.shutdown().await.unwrap();

    let mut out = String::new();
    tls.read_to_string(&mut out).await.unwrap();
    assert_eq!(
        out, "sibling saw: hello sibling",
        "plaintext must reach the sibling port unaltered, both ways"
    );

    let (up, down) = served.await.unwrap().expect("splice completed");
    assert!(up > 0 && down > 0, "bytes moved in both directions");
}

#[tokio::test]
async fn a_dead_sibling_port_answers_with_a_readable_502() {
    // Regression, prod 2026-08-16. The dial used to happen BEFORE the
    // handshake, so a dev server that was not up yet closed the connection with
    // no certificate: ERR_CONNECTION_CLOSED in a browser, SSL_ERROR_SYSCALL in
    // curl. Both read as "the platform is blocking this hostname" rather than
    // "my app is not listening", and an afternoon went into chasing an auth
    // allow-list that was never the problem.
    //
    // We own this hostname and mint a leaf the guest trusts, so the handshake
    // MUST complete and the failure MUST arrive as an HTTP response naming the
    // dead port.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ca = ca();
    let (client_side, proxy_side) = tokio::io::duplex(64 * 1024);
    let server_cfg =
        engram_egress_proxy::intercept::build_server_config(Arc::new(CertMint::new(ca.clone())));
    let served = tokio::spawn(async move {
        app_relay::serve(
            proxy_side,
            Vec::new(),
            SandboxId::new(),
            8080,
            &RefusingDialer,
            server_cfg,
        )
        .await
    });

    let mut tls = tls_client_to(&ca, APP_HOST, client_side)
        .await
        .expect("handshake completes even though the sibling port is dead");
    tls.write_all(b"GET / HTTP/1.1\r\nHost: app\r\n\r\n")
        .await
        .unwrap();

    let mut out = String::new();
    tls.read_to_string(&mut out).await.unwrap();
    assert!(
        out.starts_with("HTTP/1.1 502 Bad Gateway"),
        "a dead app port must answer, not hang up: {out}"
    );
    assert!(
        out.contains("8080"),
        "the response names the dead port: {out}"
    );

    // The connection is served, not errored — the caller logs it and moves on.
    served
        .await
        .unwrap()
        .expect("serve completes after answering");
}

#[tokio::test]
async fn the_502_survives_a_client_whose_request_is_still_in_flight() {
    // The 502 is only useful if it ARRIVES. A client sends its request right
    // after the handshake; if we answer and close while those bytes sit unread,
    // Linux emits an RST on close, and an RST lets the peer's kernel discard
    // data it has buffered but not yet handed to the application — i.e. the
    // very 502 we just wrote. So the branch drains the request first (the
    // classic HTTP "lingering close").
    //
    // duplex() has no RST semantics, so this cannot reproduce the reset itself.
    // What it CAN pin is the behaviour that prevents it: the proxy must consume
    // the client's request. The request here is deliberately larger than the
    // duplex buffer, so a proxy that never reads leaves the client's write
    // blocked forever on backpressure — the timeout below is what catches a
    // regression, instead of CI hanging.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let ca = ca();
    let (client_side, proxy_side) = tokio::io::duplex(16 * 1024);
    let server_cfg =
        engram_egress_proxy::intercept::build_server_config(Arc::new(CertMint::new(ca.clone())));
    let served = tokio::spawn(async move {
        app_relay::serve(
            proxy_side,
            Vec::new(),
            SandboxId::new(),
            5173,
            &RefusingDialer,
            server_cfg,
        )
        .await
    });

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let mut tls = tls_client_to(&ca, APP_HOST, client_side).await.unwrap();
        // 48 KiB: over the 16 KiB duplex buffer, under the drain's 64 KiB cap.
        let mut req = b"POST / HTTP/1.1\r\nHost: app\r\nContent-Length: 49152\r\n\r\n".to_vec();
        req.extend(std::iter::repeat_n(b'x', 48 * 1024));
        tls.write_all(&req)
            .await
            .expect("write buffers into rustls");
        // The flush is the load-bearing call: write_all only fills rustls'
        // internal buffer, so it completes whether or not anyone is reading.
        // Flushing pushes the records into the transport, which blocks once the
        // buffer fills unless the proxy is draining.
        tls.flush()
            .await
            .expect("the proxy drains, so the flush completes");
        let mut out = String::new();
        tls.read_to_string(&mut out).await.unwrap();
        out
    })
    .await
    .expect("the proxy must consume the request rather than leave the client blocked");

    assert!(
        outcome.starts_with("HTTP/1.1 502 Bad Gateway"),
        "the 502 still arrives after an in-flight request: {outcome}"
    );
    served
        .await
        .unwrap()
        .expect("serve completes after answering");
}
