//! `tokio_vsock`-backed [`Transport`] implementation. Linux-only —
//! vsock is a Linux kernel feature.

use std::io;

use async_trait::async_trait;
use tokio_vsock::{VsockAddr, VsockListener, VsockStream, VMADDR_CID_ANY, VMADDR_CID_HOST};

use crate::{BoxedStream, Listener, Transport};

/// `socket(AF_VSOCK)` transport. `dial` uses CID 2 (the host);
/// `listen` binds CID `VMADDR_CID_ANY` so the host can reach us
/// regardless of the hypervisor's assigned guest CID.
#[derive(Clone, Copy, Debug, Default)]
pub struct VsockTransport;

#[async_trait]
impl Transport for VsockTransport {
    async fn dial(&self, port: u32) -> io::Result<BoxedStream> {
        let stream = VsockStream::connect(VsockAddr::new(VMADDR_CID_HOST, port)).await?;
        Ok(Box::pin(stream))
    }

    async fn listen(&self, port: u32) -> io::Result<Box<dyn Listener>> {
        let listener = VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, port))?;
        Ok(Box::new(VsockListenerWrap { inner: listener }))
    }
}

struct VsockListenerWrap {
    inner: VsockListener,
}

#[async_trait]
impl Listener for VsockListenerWrap {
    async fn accept(&mut self) -> io::Result<BoxedStream> {
        let (stream, _peer) = self.inner.accept().await?;
        Ok(Box::pin(stream))
    }
}
