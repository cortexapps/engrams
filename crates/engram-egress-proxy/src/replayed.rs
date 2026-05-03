//! Stream adapter that replays a buffered prefix on read, then
//! delegates to an inner AsyncRead+AsyncWrite. Writes always go to
//! the inner stream.
//!
//! Used to stitch the SNI-peek bytes back onto the client stream
//! before handing it to rustls's TlsAcceptor — the acceptor needs to
//! see the full ClientHello from byte 0, but we already consumed
//! those bytes during the SNI peek.
//!
//! Concept matches `tokio::io::AsyncReadExt::chain` for the read
//! side, but Chain isn't AsyncWrite. This wrapper is.

use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub struct Replayed<S> {
    prefix: Vec<u8>,
    offset: usize,
    inner: S,
}

impl<S> Replayed<S> {
    pub fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix,
            offset: 0,
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Replayed<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // Drain the prefix first.
        if self.offset < self.prefix.len() {
            let remaining = self.prefix.len() - self.offset;
            let to_copy = remaining.min(buf.remaining());
            let start = self.offset;
            buf.put_slice(&self.prefix[start..start + to_copy]);
            self.offset += to_copy;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Replayed<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn replays_prefix_then_inner() {
        let (a, mut b) = tokio::io::duplex(64);
        let mut replayed = Replayed::new(b"hello ".to_vec(), a);
        // Inner channel: we'll write "world" from the other side.
        b.write_all(b"world").await.unwrap();
        drop(b);

        let mut buf = Vec::new();
        replayed.read_to_end(&mut buf).await.unwrap();
        assert_eq!(buf, b"hello world");
    }

    #[tokio::test]
    async fn writes_pass_through_to_inner() {
        let (a, mut b) = tokio::io::duplex(64);
        let mut replayed = Replayed::new(b"prefix".to_vec(), a);
        replayed.write_all(b"toinner").await.unwrap();
        let mut buf = vec![0u8; 7];
        b.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"toinner");
    }
}
