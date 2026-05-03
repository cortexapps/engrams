//! Upstream resolver for the proxy.
//!
//! When the proxy MITMs or bypasses a connection, it needs to dial
//! the *real* upstream. We dial by SNI (the hostname the guest's
//! TLS handshake claims) rather than by `SO_ORIGINAL_DST` (the IP
//! the guest chose) — the host is the TCB, so we trust the host's
//! resolution, not whatever the guest's resolver returned. This
//! also closes a DNS-rebinding-style attack: a guest can't dial
//! `evil.example.com` while sending SNI=`api.openai.com` to fool
//! the proxy into MITM'ing OpenAI bytes to evil.
//!
//! Tests inject a `StaticResolver` that maps a fixed hostname to a
//! local SocketAddr, so an in-VM e2e test can have curl hit a
//! loopback fake-upstream without owning a real DNS name.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

#[async_trait::async_trait]
pub trait UpstreamResolver: Send + Sync + std::fmt::Debug {
    async fn resolve(&self, host: &str, port: u16) -> Result<SocketAddr, ResolveError>;
}

#[derive(Debug)]
pub enum ResolveError {
    NotFound(String),
    Io(std::io::Error),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(h) => write!(f, "no addresses for `{h}`"),
            Self::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for ResolveError {}

impl From<std::io::Error> for ResolveError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Default production resolver: tokio's async getaddrinfo. Returns
/// the first IPv4 address; we only support IPv4 upstreams for now.
#[derive(Debug, Default)]
pub struct SystemResolver;

#[async_trait::async_trait]
impl UpstreamResolver for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> Result<SocketAddr, ResolveError> {
        let target = format!("{host}:{port}");
        let mut addrs = tokio::net::lookup_host(target).await?;
        for a in addrs.by_ref() {
            if a.ip().is_ipv4() {
                return Ok(a);
            }
        }
        Err(ResolveError::NotFound(host.to_string()))
    }
}

/// Test / override resolver: returns a fixed SocketAddr per host.
/// Misses fall through to NotFound (no fallback to system DNS — if
/// you've handed the proxy a static map, you've asserted the names
/// it can talk to). Hostnames are matched case-insensitively.
#[derive(Debug, Default)]
pub struct StaticResolver {
    map: HashMap<String, SocketAddr>,
}

impl StaticResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, host: impl Into<String>, addr: SocketAddr) -> Self {
        self.map.insert(host.into().to_ascii_lowercase(), addr);
        self
    }
}

#[async_trait::async_trait]
impl UpstreamResolver for StaticResolver {
    async fn resolve(&self, host: &str, _port: u16) -> Result<SocketAddr, ResolveError> {
        self.map
            .get(&host.to_ascii_lowercase())
            .copied()
            .ok_or_else(|| ResolveError::NotFound(host.to_string()))
    }
}

pub fn default_resolver() -> Arc<dyn UpstreamResolver> {
    Arc::new(SystemResolver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[tokio::test]
    async fn static_resolver_returns_mapped_address() {
        let r = StaticResolver::new().with(
            "engram-test.invalid",
            SocketAddr::from_str("127.0.0.1:8443").unwrap(),
        );
        let a = r.resolve("engram-test.invalid", 443).await.unwrap();
        assert_eq!(a.to_string(), "127.0.0.1:8443");
    }

    #[tokio::test]
    async fn static_resolver_is_case_insensitive() {
        let r = StaticResolver::new().with(
            "ExAmPlE.com",
            SocketAddr::from_str("127.0.0.1:9000").unwrap(),
        );
        assert!(r.resolve("EXAMPLE.COM", 443).await.is_ok());
    }

    #[tokio::test]
    async fn static_resolver_unknown_host_is_not_found() {
        let r = StaticResolver::new();
        let err = r.resolve("unknown.invalid", 443).await.unwrap_err();
        assert!(matches!(err, ResolveError::NotFound(_)));
    }

    #[tokio::test]
    async fn system_resolver_resolves_localhost() {
        // localhost should always resolve. Skip on platforms where
        // it doesn't (none I know of).
        let a = SystemResolver.resolve("localhost", 80).await.unwrap();
        assert_eq!(a.port(), 80);
    }
}
