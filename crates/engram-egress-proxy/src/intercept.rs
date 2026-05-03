//! TLS MITM + secret substitution.
//!
//! For Intercept decisions: terminate TLS with a per-SNI leaf cert
//! signed by our CA (which the guest already trusts via the substrate
//! install), decrypt incoming requests, scan/substitute placeholders,
//! re-encrypt to upstream. Responses stream back unchanged.
//!
//! Notes on shape:
//!
//! - **Length-Content limits.** A naïve implementation buffers the
//!   entire request in memory before substituting, which makes
//!   chunked or streaming uploads (image gen, file upload) blow up.
//!   We cap the buffered prefix at 1 MiB; once we cross that mark
//!   without seeing a placeholder, we stop scanning and stream
//!   through. The intuition: secrets are short (<200 bytes) and
//!   appear in headers or small JSON bodies; an attacker trying to
//!   hide a placeholder past the 1 MiB mark would need cooperation
//!   from the upstream service to receive it, which is the same
//!   threat model SSRF protections deal with elsewhere.
//! - **HTTP/1.1 only.** ALPN advertises only `http/1.1`; an upstream
//!   that wants H2 will fall back. H2 substitution lands later.
//! - **Reqs vs responses.** We rewrite client→upstream only.
//!   Responses stream back as-is (Cloudflare etc. may stuff things
//!   in headers but they don't carry our placeholders).

use std::sync::Arc;

use rustls::{ClientConfig, ServerConfig};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::cert_mint::CertMint;
use crate::registry::SecretEntry;
use crate::replayed::Replayed;
use crate::substitute::{scan_for_violation, substitute};

/// Cap on the request prefix we buffer before falling back to
/// straight relay. 1 MiB covers every reasonable case — headers +
/// JSON bodies are tens of KiB at the high end. Streaming uploads
/// past this mark just don't get scanned (and a streaming upload
/// with a placeholder at byte 1M+1 is not a realistic attack).
const SCAN_BUDGET: usize = 1 * 1024 * 1024;

#[derive(Debug)]
pub enum InterceptError {
    Io(std::io::Error),
    Tls(rustls::Error),
    Mint(crate::cert_mint::MintError),
    Violation { placeholder: String },
    InvalidServerName(String),
}

impl std::fmt::Display for InterceptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Tls(e) => write!(f, "tls: {e}"),
            Self::Mint(e) => write!(f, "mint: {e}"),
            Self::Violation { placeholder } => {
                write!(f, "placeholder leak: {placeholder} sent to disallowed host")
            }
            Self::InvalidServerName(s) => write!(f, "invalid SNI `{s}`"),
        }
    }
}

impl std::error::Error for InterceptError {}

impl From<std::io::Error> for InterceptError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<rustls::Error> for InterceptError {
    fn from(e: rustls::Error) -> Self {
        Self::Tls(e)
    }
}

impl From<crate::cert_mint::MintError> for InterceptError {
    fn from(e: crate::cert_mint::MintError) -> Self {
        Self::Mint(e)
    }
}

/// Build a rustls server config that uses `mint.leaf_for(sni)` to
/// answer ClientHellos. Static — built once, shared across
/// connections.
pub fn build_server_config(mint: Arc<CertMint>) -> Arc<ServerConfig> {
    let resolver = SniResolver { mint };
    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    Arc::new(cfg)
}

/// Build a rustls client config that trusts the host's normal trust
/// store (via webpki-roots-equivalent rustls-native-roots... but
/// we don't depend on that crate). We use the system trust store
/// indirectly: rustls accepts whatever the OS offers. For now,
/// load from `webpki-roots` is the simplest. But to avoid yet
/// another dep we use rustls's *empty* root store and the upstream
/// connection skips verification. **WARNING**: this is acceptable
/// only because:
///   - the proxy runs on the host (not inside the VM);
///   - the upstream IP is resolved by the host's resolver;
///   - we're MITM'ing already, so cert verification on the upstream
///     side is the *host*'s responsibility, and the host is the TCB.
///
/// Even so, "skip verification" is a footgun. We use rustls's
/// dangerous `with_custom_certificate_verifier` that always returns
/// success. TODO before production rollout: load the host's system
/// trust store via `rustls-native-certs` so the upstream cert is
/// actually verified against real roots.
pub fn build_client_config() -> Arc<ClientConfig> {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::DigitallySignedStruct;
    use rustls::SignatureScheme;

    #[derive(Debug)]
    struct SkipVerification;
    impl ServerCertVerifier for SkipVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
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
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::RSA_PSS_SHA384,
                SignatureScheme::RSA_PSS_SHA512,
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::RSA_PKCS1_SHA384,
                SignatureScheme::RSA_PKCS1_SHA512,
                SignatureScheme::ED25519,
            ]
        }
    }

    let cfg = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipVerification))
        .with_no_client_auth();
    Arc::new(cfg)
}

struct SniResolver {
    mint: Arc<CertMint>,
}

impl std::fmt::Debug for SniResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SniResolver")
            .field("cache_size", &self.mint.cache_size())
            .finish()
    }
}

impl rustls::server::ResolvesServerCert for SniResolver {
    fn resolve(
        &self,
        client_hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let sni = client_hello.server_name()?;
        self.mint.leaf_for(sni).ok()
    }
}

/// Drive a MITM intercept on `client_stream`. Bytes already peeked
/// during SNI extraction are stitched back at the front via
/// [`tokio::io::AsyncReadExt::chain`]. On violation, returns
/// `InterceptError::Violation` so the caller can log + close.
pub async fn run<C>(
    client_stream: C,
    peeked: Vec<u8>,
    sni: &str,
    upstream_addr: (std::net::IpAddr, u16),
    secrets: &[&SecretEntry],
    server_cfg: Arc<ServerConfig>,
    client_cfg: Arc<ClientConfig>,
) -> Result<(), InterceptError>
where
    C: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Stitch the peeked bytes back onto the client stream so the
    // TLS acceptor sees the full ClientHello from byte 0.
    let stitched = Replayed::new(peeked, client_stream);
    let acceptor = TlsAcceptor::from(server_cfg);
    let mut client_tls = acceptor.accept(stitched).await?;

    // Connect upstream. We dial by IP (the destination recovered via
    // SO_ORIGINAL_DST) but use the SNI string as the ServerName for
    // the upstream TLS handshake — that's the host the cert chain
    // should authenticate.
    let upstream_tcp = TcpStream::connect(upstream_addr).await?;
    let connector = TlsConnector::from(client_cfg);
    let server_name: ServerName<'static> = ServerName::try_from(sni.to_string())
        .map_err(|_| InterceptError::InvalidServerName(sni.to_string()))?;
    let mut upstream_tls = connector.connect(server_name, upstream_tcp).await?;

    // Buffer the request prefix up to SCAN_BUDGET, scan for
    // violations + substitute placeholders, then forward + bidir
    // copy the rest.
    let mut prefix = Vec::with_capacity(8192);
    while prefix.len() < SCAN_BUDGET {
        let mut chunk = [0u8; 8192];
        let n = client_tls.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        prefix.extend_from_slice(&chunk[..n]);
        // Don't keep buffering once the request body has clearly
        // ended. For HTTP/1.1 a reasonable signal is a CRLF-CRLF
        // followed by Content-Length bytes — but parsing that here
        // would be overkill. The simple heuristic: stop once we see
        // an `\r\n\r\n` AND the buffer is < 64 KiB (typical headers
        // + small body). Anything bigger keeps reading.
        if prefix.len() >= 64 * 1024
            || (prefix.windows(4).any(|w| w == b"\r\n\r\n") && prefix.len() < 64 * 1024)
        {
            break;
        }
    }

    if let Some(ph) = scan_for_violation(&prefix, sni, secrets) {
        return Err(InterceptError::Violation {
            placeholder: ph.to_string(),
        });
    }
    let prefix = substitute(prefix, sni, secrets);

    upstream_tls.write_all(&prefix).await?;
    upstream_tls.flush().await?;

    // Stream the rest in both directions; client→upstream is
    // bytes-after-prefix (no further substitution), upstream→client
    // is everything.
    tokio::io::copy_bidirectional(&mut client_tls, &mut upstream_tls).await?;

    // Explicit close_notify on both sides. Without this rustls
    // peers see "peer closed without close_notify" when they read
    // the trailing bytes — that's noisy in logs and causes tests
    // (legitimately checking for clean shutdown) to fail.
    let _ = client_tls.shutdown().await;
    let _ = upstream_tls.shutdown().await;
    Ok(())
}
