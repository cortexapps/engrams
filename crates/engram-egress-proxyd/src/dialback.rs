//! ADR 0121 §6: the app-relay dial-back.
//!
//! The ADR 0118 short circuit needs a byte stream to a port inside a
//! session's guest. Opening one takes the sandbox backend and the ADR
//! 0066 relay handshake — both live in host-agent, and keeping them
//! there keeps jail layout and the FC/VZ split out of this daemon. So
//! the daemon dials BACK: connect the dial-back UDS, name the sandbox
//! and port, and receive the already-connected stream as a raw fd via
//! SCM_RIGHTS.
//!
//! Established relayed streams survive a roll (this daemon holds the
//! fds); a NEW dial during the roll gap fails with a connection error
//! and the guest's client retries — the same semantics every other
//! new-work path has during a roll.

use std::path::PathBuf;

use async_trait::async_trait;
use engram_core::SandboxId;
use engram_egress_proxy::GuestPortDialer;

pub(crate) struct DialbackGuestPortDialer {
    sock_path: PathBuf,
}

impl DialbackGuestPortDialer {
    pub(crate) fn new(sock_path: PathBuf) -> Self {
        Self { sock_path }
    }
}

#[async_trait]
impl GuestPortDialer for DialbackGuestPortDialer {
    async fn dial(
        &self,
        sandbox: SandboxId,
        port: u16,
    ) -> std::io::Result<Box<dyn engram_egress_proxy::TunnelStream>> {
        let sock_path = self.sock_path.clone();
        // The SCM_RIGHTS receive needs a blocking std UnixStream; the
        // exchange is one tiny frame each way, then the fd.
        let fd = tokio::task::spawn_blocking(move || -> std::io::Result<std::os::fd::OwnedFd> {
            let mut sock = std::os::unix::net::UnixStream::connect(&sock_path)?;
            engram_egress_proto::write_frame_sync(
                &mut sock,
                &engram_egress_proto::DialbackRequest {
                    sandbox_id: sandbox,
                    port,
                },
            )?;
            let resp: engram_egress_proto::DialbackResponse =
                engram_egress_proto::read_frame_sync(&mut sock)?;
            match resp {
                engram_egress_proto::DialbackResponse::Ok => engram_egress_proto::recv_fd(&sock),
                engram_egress_proto::DialbackResponse::Err(e) => {
                    Err(std::io::Error::other(format!("dial-back refused: {e}")))
                }
            }
        })
        .await
        .map_err(|e| std::io::Error::other(format!("dial-back task: {e}")))??;

        let std_stream = std::os::unix::net::UnixStream::from(fd);
        std_stream.set_nonblocking(true)?;
        let stream = tokio::net::UnixStream::from_std(std_stream)?;
        Ok(Box::new(stream))
    }
}
