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
use crate::registry::{Decision, InjectRefresher, Registry};
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
    /// Where the Google metadata-compatible ADC endpoint listens. `None`
    /// disables it.
    pub metadata_bind_addr: Option<SocketAddr>,
    /// Upstream resolver the DNS proxy forwards allowed queries to.
    /// Defaults to Cloudflare's 1.1.1.1:53.
    pub dns_upstream: SocketAddr,
    /// ADR 0056 (Phase 4): sink for observed `IntegrationAsset`s. `None`
    /// disables observation regardless of policy (no consumer to forward to).
    /// The host-agent wires this to its coordinator bridge; tests pass a
    /// collecting closure.
    pub observe_sink: Option<crate::observe::ObserveSink>,
    /// WS4: seam to re-mint a near-expiry inject credential via the coordinator.
    /// `None` disables refresh (a minted inject then rides its boot token until
    /// expiry — the pre-WS4 behaviour). The host-agent wires this to its coord
    /// client; tests pass a stub.
    pub inject_refresher: Option<Arc<dyn InjectRefresher>>,
    /// Extra trust roots for hermetic full-network tests. Production leaves
    /// this empty and uses the built-in WebPKI roots.
    pub upstream_test_roots: Option<rustls::RootCertStore>,
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
            metadata_bind_addr: None,
            dns_upstream: dns::DEFAULT_UPSTREAM
                .parse()
                .expect("dns upstream default parses"),
            observe_sink: None,
            inject_refresher: None,
            upstream_test_roots: None,
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
        let client_cfg = cfg.upstream_test_roots.clone().map_or_else(
            intercept::build_client_config,
            intercept::build_client_config_with_roots,
        );
        Self {
            cfg,
            server_cfg,
            client_cfg,
        }
    }

    /// Bind the 443 listener and (when configured) the DNS sockets,
    /// returning just the sockets for [`serve`](Self::serve) to run.
    ///
    /// Bind is split from serve so a bind failure is a value the caller
    /// can act on. Egress is mandatory (issue #240): a host-agent that
    /// keeps running with a live iptables `:443 -> proxy` REDIRECT but
    /// no listener sends every guest a RST (the guest's TLS client
    /// reports `ConnectionRefused`), silently breaking every session on
    /// the host. A DNS bind failure is just as fatal — the guest's
    /// `:53 -> dns` REDIRECT is live too, so a dead DNS socket breaks
    /// resolution the same way.
    pub async fn bind(&self) -> Result<Listeners, std::io::Error> {
        let tcp = TcpListener::bind(self.cfg.bind_addr).await?;
        let dns = if let Some(dns_addr) = self.cfg.dns_bind_addr {
            let udp = UdpSocket::bind(dns_addr).await.inspect_err(|e| {
                tracing::error!(addr = %dns_addr, error = %e, "DNS/udp bind failed");
            })?;
            let dns_tcp = TcpListener::bind(dns_addr).await.inspect_err(|e| {
                tracing::error!(addr = %dns_addr, error = %e, "DNS/tcp bind failed");
            })?;
            Some((Arc::new(udp), dns_tcp))
        } else {
            None
        };
        let metadata = match self.cfg.metadata_bind_addr {
            Some(addr) => Some(TcpListener::bind(addr).await.inspect_err(|error| {
                tracing::error!(%addr, %error, "metadata bind failed");
            })?),
            None => None,
        };
        Ok(Listeners { tcp, dns, metadata })
    }

    /// Run the accept loop forever on already-bound listeners. The DNS
    /// serve loops are spawned as child tasks. Returns only if the
    /// accept loop itself terminates (it shouldn't — accept errors are
    /// logged and retried).
    pub async fn serve(self, listeners: Listeners) {
        let Listeners { tcp, dns, metadata } = listeners;
        tracing::info!(addr = ?tcp.local_addr().ok(), "engram-egress-proxy listening");

        // The filtering DNS proxy: serve loops for the already-bound
        // udp/53 + tcp/53 sockets. iptables REDIRECTs guest DNS traffic
        // here regardless of the upstream IP they pick.
        if let Some((udp, dns_tcp)) = dns {
            let upstream = self.cfg.dns_upstream;
            let registry_for_udp = self.cfg.registry.clone();
            tokio::spawn(async move {
                if let Err(e) = dns::serve_udp(udp, registry_for_udp, upstream).await {
                    tracing::error!(error = %e, "DNS/udp serve loop ended");
                }
            });
            let registry_for_tcp = self.cfg.registry.clone();
            tokio::spawn(async move {
                if let Err(e) = dns::serve_tcp(dns_tcp, registry_for_tcp, upstream).await {
                    tracing::error!(error = %e, "DNS/tcp serve loop ended");
                }
            });
        }
        if let Some(listener) = metadata {
            let registry = self.cfg.registry.clone();
            tokio::spawn(async move {
                if let Err(error) = crate::metadata::serve(listener, registry).await {
                    tracing::error!(%error, "metadata serve loop ended");
                }
            });
        }

        loop {
            let (stream, peer) = match tcp.accept().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    continue;
                }
            };
            let registry = self.cfg.registry.clone();
            let resolver = self.cfg.resolver.clone();
            let server_cfg = self.server_cfg.clone();
            let client_cfg = self.client_cfg.clone();
            let observe_sink = self.cfg.observe_sink.clone();
            let inject_refresher = self.cfg.inject_refresher.clone();
            tokio::spawn(async move {
                if let Err(e) = handle(
                    stream,
                    peer,
                    registry,
                    resolver,
                    server_cfg,
                    client_cfg,
                    observe_sink,
                    inject_refresher,
                )
                .await
                {
                    tracing::debug!(peer = %peer, error = %e, "connection handler ended with error");
                }
            });
        }
    }
}

/// The bound listeners handed from [`Proxy::bind`] to [`Proxy::serve`]:
/// the 443 TCP listener and, when the DNS filter is enabled, its
/// UDP+TCP sockets. An opaque token — all proxy state stays on `Proxy`.
pub struct Listeners {
    tcp: TcpListener,
    dns: Option<(Arc<UdpSocket>, TcpListener)>,
    metadata: Option<TcpListener>,
}

impl Listeners {
    /// The address the 443 listener actually bound (useful when the
    /// config asked for an ephemeral port).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.tcp.local_addr()
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle(
    mut stream: TcpStream,
    peer: SocketAddr,
    registry: Arc<Registry>,
    resolver: Arc<dyn UpstreamResolver>,
    server_cfg: Arc<rustls::ServerConfig>,
    client_cfg: Arc<rustls::ClientConfig>,
    observe_sink: Option<crate::observe::ObserveSink>,
    inject_refresher: Option<Arc<dyn InjectRefresher>>,
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

    // WS4 (campaign C2 audit — empty-body-looks-like-204 hazard): every reject
    // path below closes the connection WITHOUT writing any HTTP bytes to the
    // guest — `Reject` before any TLS at all, `RequestRejected`/`GraphqlRejected`
    // after the guest-side TLS handshake but before a single response byte. The
    // guest's HTTP client sees an unambiguous connection close (curl: "empty
    // reply from server"; gh: transport error), NEVER a synthesized 2xx/204. And
    // the intercept path streams the upstream response VERBATIM
    // (`copy_bidirectional` / `pump_and_observe` — no status/body synthesis), so a
    // real upstream 401 reaches the guest AS a 401. Therefore the campaign's
    // "branch DELETE returned an empty body indistinguishable from 204 while the
    // branch survived" was NOT produced here: the DELETE matched its inject policy
    // (a permitted write), so it was forwarded with the boot-time injected token
    // that had gone stale ~1h in — GitHub 401'd it, and `gh`'s own handling
    // rendered that 401 as an empty body. The root cause is the stale token, fixed
    // by the TTL-aware re-mint above (`InjectEntry::refresh_if_stale`), not a
    // proxy-synthesized success. No proxy-side fix is warranted; do NOT add a path
    // here that forwards or synthesizes an empty success-shaped reply.
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
        Decision::Intercept {
            secrets,
            injects,
            observes,
            foreign_placeholders,
        } => {
            let result = intercept::run(
                stream,
                peeked,
                resolver,
                intercept::StreamContext {
                    sni: &sni,
                    port,
                    secrets: &secrets,
                    injects: &injects,
                    observes: &observes,
                    foreign_placeholders: &foreign_placeholders,
                    session_id: session.session_id,
                    sink: observe_sink.as_ref(),
                    refresher: inject_refresher.as_deref(),
                },
                server_cfg,
                client_cfg,
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
                // ADR 0056: an inject-gated host got a request shape no policy
                // permits — close it (the side effect never reaches upstream),
                // same disposition as a placeholder leak.
                Err(intercept::InterceptError::RequestRejected { method, path }) => {
                    tracing::info!(
                        session_id = %session.session_id,
                        sni = %sni, %method, %path,
                        "egress rejected — request shape not permitted by integration policy",
                    );
                    Ok(())
                }
                // ADR 0059: a GraphQL request to a gated endpoint whose operation
                // isn't permitted (or whose body was unparseable / over-cap) — close
                // it (nothing reached upstream), same disposition as a REST reject.
                Err(intercept::InterceptError::GraphqlRejected { reason }) => {
                    tracing::info!(
                        session_id = %session.session_id,
                        sni = %sni, reason,
                        "egress rejected — graphql operation not permitted by integration policy",
                    );
                    Ok(())
                }
                Err(intercept::InterceptError::CredentialRequestRejected { method, path }) => {
                    tracing::warn!(
                        session_id = %session.session_id,
                        target = %sni,
                        %method,
                        %path,
                        outcome = "denied",
                        "egress rejected credential-producing Google API request",
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
