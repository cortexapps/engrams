//! Bypass relay — splice bytes between guest and upstream without
//! decrypting.
//!
//! Used when the destination is allowed by `network.allow_hosts` but
//! no `[secrets.X]` allowlist matches, so there's nothing to
//! substitute and no reason to MITM. We replay the SNI peek bytes
//! upstream and then bidirectional-copy until both halves close.
//!
//! Cheap: no allocation per byte, no TLS state. The proxy just sits
//! on the connection's critical path long enough to authorize via
//! SNI; from there it's a kernel-side pipe.

use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::resolver::{ResolveError, UpstreamResolver};

/// Splice `client` ↔ `upstream`. The proxy dials upstream by SNI
/// (resolved via `resolver`) — *not* by `SO_ORIGINAL_DST`. This
/// trusts the host's resolver, which is the TCB; a compromised
/// guest can't swap the upstream IP under us. `peeked` are the
/// bytes we already read off `client` during SNI peek — they get
/// sent to upstream before the bidirectional copy starts so
/// upstream sees a complete TLS handshake from byte 0.
pub async fn relay<C>(
    mut client: C,
    sni: &str,
    port: u16,
    resolver: Arc<dyn UpstreamResolver>,
    peeked: Vec<u8>,
) -> Result<(u64, u64), BypassError>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    let upstream_addr = resolver.resolve(sni, port).await?;
    let mut upstream = TcpStream::connect(upstream_addr).await?;
    upstream.write_all(&peeked).await?;
    upstream.flush().await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream)
        .await
        .map_err(BypassError::Io)
}

#[derive(Debug)]
pub enum BypassError {
    Io(std::io::Error),
    Resolve(ResolveError),
}

impl std::fmt::Display for BypassError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Resolve(e) => write!(f, "resolve: {e}"),
        }
    }
}

impl std::error::Error for BypassError {}

impl From<std::io::Error> for BypassError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<ResolveError> for BypassError {
    fn from(e: ResolveError) -> Self {
        Self::Resolve(e)
    }
}
