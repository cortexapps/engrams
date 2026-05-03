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

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// Splice `client` ↔ `upstream`. `peeked` are the bytes we already
/// read off `client` during SNI peek — they get sent to the upstream
/// before the bidirectional copy starts so the upstream sees a
/// complete TLS handshake from byte 0.
pub async fn relay<C>(
    mut client: C,
    upstream_addr: (std::net::IpAddr, u16),
    peeked: Vec<u8>,
) -> Result<(u64, u64), std::io::Error>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    let mut upstream = TcpStream::connect(upstream_addr).await?;
    upstream.write_all(&peeked).await?;
    upstream.flush().await?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await
}
