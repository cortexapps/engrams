//! Accept loop + per-connection dispatcher.
//!
//! Listens on `bind_addr` (TCP) for connections REDIRECTed by
//! iptables out of the FC TAPs. For each accepted connection:
//!
//! 1. Recover the original destination via `SO_ORIGINAL_DST`. (Linux
//!    only — on macOS we just use the local addr; the proxy doesn't
//!    actually run on macOS, that's compile-only.)
//! 2. Recover the source IP via `peer_addr`. Look up the matching
//!    `SessionState` from the registry. No match → drop.
//! 3. Peek the SNI (TLS) or the HTTP `Host:` (plain HTTP — TODO).
//!    For now, this proxy is HTTPS-only; iptables shouldn't
//!    REDIRECT port 80 yet.
//! 4. Decide via `SessionState::decide(sni)`:
//!      - Reject → close.
//!      - Bypass → splice via `bypass::relay`.
//!      - Intercept → MITM via `intercept::run`.
//!
//! Errors are logged but not propagated; the proxy is a best-effort
//! dispatcher. The most important failure mode is "violation" (a
//! placeholder appearing where it shouldn't) — those get a `WARN`
//! log so operators see them.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream, UdpSocket};

use crate::cert_mint::CertMint;
use crate::registry::{Decision, Registry};
use crate::resolver::{default_resolver, UpstreamResolver};
use crate::{bypass, dns, intercept, sni};

/// How long we'll spend reading the TLS ClientHello before giving up.
const SNI_PEEK_BUDGET: Duration = Duration::from_secs(5);
/// Max bytes we'll buffer of the ClientHello.
const SNI_PEEK_BYTES: usize = 16 * 1024;

pub struct ProxyConfig {
    /// Where to listen for REDIRECTed traffic. Typically
    /// `0.0.0.0:9443` with iptables redirecting VM TAP traffic to
    /// this port.
    pub bind_addr: SocketAddr,
    pub registry: Arc<Registry>,
    pub mint: Arc<CertMint>,
    /// Resolver for upstream SNI → SocketAddr. Defaults to
    /// `SystemResolver` (host's DNS). Tests pass a `StaticResolver`
    /// to point a fixed hostname at a loopback fixture.
    pub resolver: Arc<dyn UpstreamResolver>,
    /// Where the filtering DNS proxy listens (both UDP and TCP).
    /// `None` disables the DNS path entirely — iptables would then
    /// need to keep the unconditional `ACCEPT VM→1.1.1.1:53` rules
    /// so the guest can resolve at all, accepting the DNS-exfil
    /// channel. Production sets `Some(0.0.0.0:53)` and pairs it
    /// with iptables `REDIRECT VM→{udp,tcp}/53 → :53`.
    pub dns_bind_addr: Option<SocketAddr>,
    /// Upstream resolver the DNS proxy forwards allowed queries to.
    /// Defaults to Cloudflare's 1.1.1.1:53.
    pub dns_upstream: SocketAddr,
}

impl ProxyConfig {
    /// Convenience constructor: production wiring with the system
    /// DNS resolver and the bind addr / registry / mint the caller
    /// provides. DNS filter on by default at port 53 with 1.1.1.1
    /// upstream — disable by setting `dns_bind_addr = None`.
    pub fn new(bind_addr: SocketAddr, registry: Arc<Registry>, mint: Arc<CertMint>) -> Self {
        Self {
            bind_addr,
            registry,
            mint,
            resolver: default_resolver(),
            // 5353 by default — avoids the systemd-resolved bind on
            // 127.0.0.53:53 on hosts that run it. Override via the
            // host-agent's `--egress-dns-port`. Iptables REDIRECTs
            // guest {udp,tcp}/53 to this port.
            dns_bind_addr: Some("0.0.0.0:5353".parse().expect("dns bind default parses")),
            dns_upstream: dns::DEFAULT_UPSTREAM
                .parse()
                .expect("dns upstream default parses"),
        }
    }
}

pub struct Proxy {
    cfg: ProxyConfig,
    server_cfg: Arc<rustls::ServerConfig>,
    client_cfg: Arc<rustls::ClientConfig>,
}

impl Proxy {
    pub fn new(cfg: ProxyConfig) -> Self {
        let server_cfg = intercept::build_server_config(cfg.mint.clone());
        let client_cfg = intercept::build_client_config();
        Self {
            cfg,
            server_cfg,
            client_cfg,
        }
    }

    /// Run forever. Returns only on listener bind failure.
    pub async fn run(self) -> Result<(), std::io::Error> {
        let listener = TcpListener::bind(self.cfg.bind_addr).await?;
        tracing::info!(addr = %self.cfg.bind_addr, "engram-egress-proxy listening");
        let registry = self.cfg.registry.clone();
        let resolver = self.cfg.resolver.clone();
        let server_cfg = self.server_cfg.clone();
        let client_cfg = self.client_cfg.clone();

        // Spawn the filtering DNS proxy. Bound on udp/53 + tcp/53
        // (via the same dns_bind_addr); iptables REDIRECTs guest
        // DNS traffic here regardless of the upstream IP they pick.
        if let Some(dns_addr) = self.cfg.dns_bind_addr {
            let udp_sock = match UdpSocket::bind(dns_addr).await {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    tracing::error!(addr = %dns_addr, error = %e, "DNS/udp bind failed");
                    return Err(e);
                }
            };
            let tcp_listener = match TcpListener::bind(dns_addr).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(addr = %dns_addr, error = %e, "DNS/tcp bind failed");
                    return Err(e);
                }
            };
            let upstream = self.cfg.dns_upstream;
            let registry_for_udp = registry.clone();
            tokio::spawn(async move {
                if let Err(e) = dns::serve_udp(udp_sock, registry_for_udp, upstream).await {
                    tracing::error!(error = %e, "DNS/udp serve loop ended");
                }
            });
            let registry_for_tcp = registry.clone();
            tokio::spawn(async move {
                if let Err(e) = dns::serve_tcp(tcp_listener, registry_for_tcp, upstream).await {
                    tracing::error!(error = %e, "DNS/tcp serve loop ended");
                }
            });
        }

        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    continue;
                }
            };
            let registry = registry.clone();
            let resolver = resolver.clone();
            let server_cfg = server_cfg.clone();
            let client_cfg = client_cfg.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    handle(stream, peer, registry, resolver, server_cfg, client_cfg).await
                {
                    tracing::debug!(peer = %peer, error = %e, "connection handler ended with error");
                }
            });
        }
    }
}

async fn handle(
    mut stream: TcpStream,
    peer: SocketAddr,
    registry: Arc<Registry>,
    resolver: Arc<dyn UpstreamResolver>,
    server_cfg: Arc<rustls::ServerConfig>,
    client_cfg: Arc<rustls::ClientConfig>,
) -> Result<(), HandleError> {
    let guest_ip = match peer.ip() {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(_) => return Err(HandleError::NoSession),
    };
    let session = registry.lookup(guest_ip).ok_or(HandleError::NoSession)?;

    // Pre-REDIRECT destination — useful only for logs. We don't
    // dial it; the upstream is resolved fresh by SNI on the host.
    // On non-Linux this errors (no SO_ORIGINAL_DST) — fine, we
    // just log "unknown".
    let original_dst = original_destination(&stream).ok();

    let (sni, peeked) = sni::peek_sni(&mut stream, SNI_PEEK_BYTES, SNI_PEEK_BUDGET)
        .await
        .map_err(HandleError::SniPeek)?;
    // The proxy only handles port 443 today (iptables only REDIRECTs
    // tcp/443). The port the upstream listens on is the same.
    let port = original_dst.map(|(_, p)| p).unwrap_or(443);

    match session.decide(&sni) {
        Decision::Reject => {
            tracing::info!(
                session_id = %session.session_id,
                sni = %sni,
                original_dst = ?original_dst,
                "egress rejected — destination not in any allow_hosts",
            );
            Ok(())
        }
        Decision::Bypass => {
            let (up, down) = bypass::relay(stream, &sni, port, resolver, peeked)
                .await
                .map_err(HandleError::Bypass)?;
            tracing::debug!(
                session_id = %session.session_id,
                sni = %sni,
                bytes_up = up,
                bytes_down = down,
                "egress bypass complete",
            );
            Ok(())
        }
        Decision::Intercept(secrets) => {
            let result = intercept::run(
                stream, peeked, &sni, port, resolver, &secrets, server_cfg, client_cfg,
            )
            .await;
            match result {
                Ok(()) => Ok(()),
                Err(intercept::InterceptError::Violation { placeholder }) => {
                    tracing::warn!(
                        session_id = %session.session_id,
                        sni = %sni,
                        placeholder,
                        "VIOLATION: placeholder sent to host outside its allow_hosts",
                    );
                    Ok(())
                }
                Err(e) => Err(HandleError::Intercept(e)),
            }
        }
    }
}

#[derive(Debug)]
enum HandleError {
    NoSession,
    SniPeek(crate::sni::PeekError),
    Bypass(crate::bypass::BypassError),
    Intercept(crate::intercept::InterceptError),
    OriginalDest(std::io::Error),
}

impl std::fmt::Display for HandleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSession => write!(f, "no session for source IP"),
            Self::SniPeek(e) => write!(f, "sni peek: {e}"),
            Self::Bypass(e) => write!(f, "bypass: {e}"),
            Self::Intercept(e) => write!(f, "intercept: {e}"),
            Self::OriginalDest(e) => write!(f, "SO_ORIGINAL_DST: {e}"),
        }
    }
}

impl std::error::Error for HandleError {}

/// Recover the original destination of a REDIRECTed connection via
/// `getsockopt(SOL_IP, SO_ORIGINAL_DST)`, exposed by socket2 as
/// `SockRef::original_dst()`. Linux-only; on macOS the call returns
/// `Unsupported` (the proxy doesn't actually run on macOS, but this
/// branch keeps `cargo check --workspace` clean).
fn original_destination(stream: &TcpStream) -> Result<(IpAddr, u16), HandleError> {
    #[cfg(target_os = "linux")]
    {
        let sock = socket2::SockRef::from(stream);
        let dst = sock.original_dst_v4().map_err(HandleError::OriginalDest)?;
        if let Some(v4) = dst.as_socket_ipv4() {
            return Ok((IpAddr::V4(*v4.ip()), v4.port()));
        }
        Err(HandleError::OriginalDest(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "non-IPv4 original destination",
        )))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = stream;
        Err(HandleError::OriginalDest(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "SO_ORIGINAL_DST is Linux-only",
        )))
    }
}
