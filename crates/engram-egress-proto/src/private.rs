//! Host-private network transport. The connected stream is the capability.
//! No value in a guest packet selects an allocation or session.
use engram_core::{HostId, SandboxId, SessionId};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{io, net::Ipv4Addr};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const VERSION: u32 = 1;
pub const MAX_HEADER: usize = 2048;
pub const MAX_DNS: usize = 4096;
pub const MAX_FLOWS: usize = 64;
pub const MAX_DNS_FLOWS: usize = 8;
pub const HANDOFF_SECONDS: u64 = 5;

/// Host-only registration, checked against the live proxy and policy. The
/// placement epoch is deliberately absent: registration is always CLOSED.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Allocation {
    pub version: u32,
    pub host_id: HostId,
    pub host_incarnation: String,
    pub proxy_incarnation: String,
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
    pub boot_attempt: String,
    pub attachment_generation: u64,
    pub guest_ip: Ipv4Addr,
    pub gateway_ip: Ipv4Addr,
    pub policy_revision: u64,
    pub policy_digest: String,
}
impl Allocation {
    pub fn validate(&self) -> io::Result<()> {
        let bounded = |s: &str| {
            !s.is_empty()
                && s.len() <= 128
                && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        };
        if self.version != VERSION
            || self.host_id.as_uuid().is_nil()
            || self.session_id.as_uuid().is_nil()
            || self.sandbox_id.as_uuid().is_nil()
            || !bounded(&self.host_incarnation)
            || !bounded(&self.proxy_incarnation)
            || !bounded(&self.boot_attempt)
            || self.attachment_generation == 0
            || self.guest_ip == self.gateway_ip
            || self.guest_ip.is_unspecified()
            || self.guest_ip.is_multicast()
            || self.gateway_ip.is_unspecified()
            || self.gateway_ip.is_multicast()
            || !engram_core::types::managed_deny::is_digest(&self.policy_digest)
        {
            return Err(invalid());
        }
        Ok(())
    }
}

/// Strict monotonic sequence on one allocation stream. Bind can occur once.
/// A TCP or DNS header is immediately followed by one SCM_RIGHTS carrier.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub sequence: u64,
    pub epoch: u64,
    pub operation: Operation,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Bind,
    Tcp { destination: Ipv4Addr, port: u16 },
    Dns { destination: Ipv4Addr, port: u16 },
    Close,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub version: u32,
    pub sequence: u64,
    pub epoch: u64,
    pub state: String,
}

pub fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "invalid private network transport",
    )
}
pub async fn read<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let mut prefix = [0; 4];
    r.read_exact(&mut prefix[..1]).await?;
    tokio::time::timeout(std::time::Duration::from_secs(HANDOFF_SECONDS), async {
        r.read_exact(&mut prefix[1..]).await?;
        let len = u32::from_be_bytes(prefix) as usize;
        if len == 0 || len > MAX_HEADER {
            return Err(invalid());
        }
        let mut body = vec![0; len];
        r.read_exact(&mut body).await?;
        serde_json::from_slice(&body).map_err(|_| invalid())
    })
    .await
    .map_err(|_| invalid())?
}
pub async fn write<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, v: &T) -> io::Result<()> {
    let body = serde_json::to_vec(v).map_err(|_| invalid())?;
    if body.is_empty() || body.len() > MAX_HEADER {
        return Err(invalid());
    }
    tokio::time::timeout(std::time::Duration::from_secs(HANDOFF_SECONDS), async {
        w.write_all(&(body.len() as u32).to_be_bytes()).await?;
        w.write_all(&body).await?;
        w.flush().await
    })
    .await
    .map_err(|_| invalid())?
}

/// A host allocation and its registered reliable stream are inseparable until
/// the backend transfers ownership to its helper. No unchecked constructor.
pub struct RegisteredPrivateNetwork {
    allocation: Allocation,
    stream: tokio::net::UnixStream,
}
impl RegisteredPrivateNetwork {
    pub fn allocation(&self) -> &Allocation {
        &self.allocation
    }
    pub fn into_stream(self) -> tokio::net::UnixStream {
        self.stream
    }
}

/// Register a fresh capability on a dedicated host control connection. Never
/// retry: loss of the reply makes this attempt unusable. The returned socket is
/// for one helper only and starts closed. It must not be cached across boots.
pub async fn register(
    mut stream: tokio::net::UnixStream,
    allocation: &Allocation,
) -> io::Result<RegisteredPrivateNetwork> {
    allocation.validate()?;
    crate::write_frame(&mut stream, &crate::ToProxyd::PrivateNetwork).await?;
    write(&mut stream, allocation).await?;
    let reply: Reply = tokio::time::timeout(
        std::time::Duration::from_secs(HANDOFF_SECONDS),
        read(&mut stream),
    )
    .await
    .map_err(|_| invalid())??;
    if reply
        != (Reply {
            version: VERSION,
            sequence: 0,
            epoch: 0,
            state: "closed".into(),
        })
    {
        return Err(invalid());
    }
    Ok(RegisteredPrivateNetwork {
        allocation: allocation.clone(),
        stream,
    })
}

/// Receive exactly one connected Unix stream, with close-on-exec set before it
/// leaves this function. Close every installed descriptor on malformed input.
#[cfg(unix)]
pub async fn receive_stream(socket: &tokio::net::UnixStream) -> io::Result<tokio::net::UnixStream> {
    use nix::sys::socket::{self, ControlMessageOwned, MsgFlags};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let fd = socket
        .async_io(tokio::io::Interest::READABLE, || {
            let mut carrier = [0u8; 1];
            let mut iov = [io::IoSliceMut::new(&mut carrier)];
            // Receive the OS maximum, then reject every count except one.
            // XNU UIPC_MAX_CMSG_FD is 512; Linux SCM_MAX_FD is 253.
            // A smaller buffer can leak unreported descriptors on Darwin:
            // unp_externalize installs all rights before copyout truncation.
            // https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/uipc_usrreq.c
            let mut control = nix::cmsg_space!([std::os::fd::RawFd; 512]);
            #[cfg(target_os = "linux")]
            let flags = MsgFlags::MSG_CMSG_CLOEXEC;
            #[cfg(not(target_os = "linux"))]
            let flags = MsgFlags::empty();
            let msg =
                socket::recvmsg::<()>(socket.as_raw_fd(), &mut iov, Some(&mut control), flags)
                    .map_err(io::Error::from)?;
            let mut fds = Vec::new();
            for message in msg.cmsgs().map_err(io::Error::from)? {
                if let ControlMessageOwned::ScmRights(raw) = message {
                    for fd in raw {
                        // SAFETY: the kernel installed this descriptor. It is
                        // wrapped once and dropped even on a rejected message.
                        fds.push(unsafe { OwnedFd::from_raw_fd(fd) });
                    }
                }
            }
            let bad = msg.bytes != 1
                || msg
                    .flags
                    .intersects(MsgFlags::MSG_CTRUNC | MsgFlags::MSG_TRUNC);
            if bad || carrier[0] != 0xED || fds.len() != 1 {
                return Err(invalid());
            }
            let fd = fds.pop().ok_or_else(invalid)?;
            // SAFETY: fd is live and owned. F_SETFD changes only its descriptor flag.
            if unsafe {
                nix::libc::fcntl(fd.as_raw_fd(), nix::libc::F_SETFD, nix::libc::FD_CLOEXEC)
            } == -1
            {
                return Err(io::Error::last_os_error());
            }
            if socket::getsockopt(&fd, socket::sockopt::SockType).map_err(io::Error::from)?
                != socket::SockType::Stream
            {
                return Err(invalid());
            }
            socket::getpeername::<socket::UnixAddr>(fd.as_raw_fd()).map_err(io::Error::from)?;
            Ok(fd)
        })
        .await?;
    let stream = std::os::unix::net::UnixStream::from(fd);
    stream.set_nonblocking(true)?;
    tokio::net::UnixStream::from_std(stream)
}

/// Drain canceled headers and descriptor carriers until the helper confirms
/// cleanup by closing its reliable private stream. The caller bounds the wait.
#[cfg(unix)]
pub async fn drain_to_eof(socket: &tokio::net::UnixStream) -> io::Result<()> {
    use nix::sys::socket::{self, ControlMessageOwned, MsgFlags};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    loop {
        let bytes = socket
            .async_io(tokio::io::Interest::READABLE, || {
                let mut bytes = [0; MAX_HEADER];
                let mut iov = [io::IoSliceMut::new(&mut bytes)];
                // Keep the same OS-maximum ancillary capacity as receive_stream.
                let mut control = nix::cmsg_space!([std::os::fd::RawFd; 512]);
                #[cfg(target_os = "linux")]
                let flags = MsgFlags::MSG_CMSG_CLOEXEC;
                #[cfg(not(target_os = "linux"))]
                let flags = MsgFlags::empty();
                let message =
                    socket::recvmsg::<()>(socket.as_raw_fd(), &mut iov, Some(&mut control), flags)
                        .map_err(io::Error::from)?;
                for message in message.cmsgs().map_err(io::Error::from)? {
                    if let ControlMessageOwned::ScmRights(fds) = message {
                        for fd in fds {
                            // SAFETY: each received descriptor is newly installed
                            // by the kernel and is closed here exactly once.
                            drop(unsafe { OwnedFd::from_raw_fd(fd) });
                        }
                    }
                }
                if message
                    .flags
                    .intersects(MsgFlags::MSG_CTRUNC | MsgFlags::MSG_TRUNC)
                {
                    return Err(invalid());
                }
                Ok(message.bytes)
            })
            .await?;
        if bytes == 0 {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    #[tokio::test]
    async fn registration_keeps_exact_allocation_and_rejects_wrong_closed_reply() {
        let allocation = Allocation {
            version: 1,
            host_id: HostId::new(),
            host_incarnation: "host-1".into(),
            proxy_incarnation: "proxy-1".into(),
            session_id: SessionId::new(),
            sandbox_id: SandboxId::new(),
            boot_attempt: "boot-1".into(),
            attachment_generation: 1,
            guest_ip: "192.0.2.2".parse().unwrap(),
            gateway_ip: "192.0.2.1".parse().unwrap(),
            policy_revision: 1,
            policy_digest: format!("sha256:{}", "a".repeat(64)),
        };
        for epoch in [0, 1] {
            let (a, mut b) = tokio::net::UnixStream::pair().unwrap();
            let expected = allocation.clone();
            let peer = tokio::spawn(async move {
                assert!(matches!(
                    crate::read_frame::<_, crate::ToProxyd>(&mut b)
                        .await
                        .unwrap(),
                    crate::ToProxyd::PrivateNetwork
                ));
                let sent: Allocation = read(&mut b).await.unwrap();
                assert_eq!(sent, expected);
                write(
                    &mut b,
                    &Reply {
                        version: 1,
                        sequence: 0,
                        epoch,
                        state: "closed".into(),
                    },
                )
                .await
                .unwrap();
                b
            });
            let result = register(a, &allocation).await;
            let mut peer = peer.await.unwrap();
            if epoch == 0 {
                let registered = result.unwrap();
                assert_eq!(registered.allocation(), &allocation);
                // The stream remains the exact capability, owned until consumed.
                let mut capability = registered.into_stream();
                capability.write_all(b"x").await.unwrap();
                let mut byte = [0];
                peer.read_exact(&mut byte).await.unwrap();
                assert_eq!(byte, *b"x");
            } else {
                assert!(result.is_err());
                assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
            }
        }
    }

    #[tokio::test]
    async fn oversized_private_header_is_rejected_before_body_read() {
        let (mut a, mut b) = tokio::net::UnixStream::pair().unwrap();
        a.write_all(&2049u32.to_be_bytes()).await.unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            read::<_, Request>(&mut b),
        )
        .await;
        assert!(matches!(result, Ok(Err(e)) if e.kind() == io::ErrorKind::InvalidData));
    }
    #[tokio::test]
    async fn descriptor_handoff_rejects_wrong_types_and_closes_all_extra_fds() {
        use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
        use std::os::fd::{AsFd, AsRawFd};
        let (sender, receiver) = tokio::net::UnixStream::pair().unwrap();
        let file = tempfile::tempfile().unwrap();
        let raw = sender.as_fd().try_clone_to_owned().unwrap();
        crate::send_fd(&std::os::unix::net::UnixStream::from(raw), file.as_fd()).unwrap();
        assert!(receive_stream(&receiver).await.is_err());
        let (mut a, b) = tokio::net::UnixStream::pair().unwrap();
        let (mut c, d) = tokio::net::UnixStream::pair().unwrap();
        let fds = [b.as_raw_fd(), d.as_raw_fd()];
        let control = [ControlMessage::ScmRights(&fds)];
        sendmsg::<()>(
            sender.as_raw_fd(),
            &[io::IoSlice::new(&[0xED])],
            &control,
            MsgFlags::empty(),
            None,
        )
        .unwrap();
        drop(b);
        drop(d);
        assert!(receive_stream(&receiver).await.is_err());
        let mut byte = [0];
        assert_eq!(a.read(&mut byte).await.unwrap(), 0);
        assert_eq!(c.read(&mut byte).await.unwrap(), 0);
    }
    #[tokio::test]
    async fn oversized_rights_message_closes_every_installed_descriptor() {
        use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
        use std::os::fd::AsRawFd;
        let (sender, receiver) = tokio::net::UnixStream::pair().unwrap();
        let mut peers = Vec::new();
        let mut passed = Vec::new();
        for _ in 0..32 {
            let (a, b) = tokio::net::UnixStream::pair().unwrap();
            peers.push(a);
            passed.push(b);
        }
        let fds: Vec<_> = passed.iter().map(AsRawFd::as_raw_fd).collect();
        sendmsg::<()>(
            sender.as_raw_fd(),
            &[io::IoSlice::new(&[0xED])],
            &[ControlMessage::ScmRights(&fds)],
            MsgFlags::empty(),
            None,
        )
        .unwrap();
        drop(passed);
        assert!(receive_stream(&receiver).await.is_err());
        for (index, mut peer) in peers.into_iter().enumerate() {
            let mut byte = [0];
            assert!(
                matches!(
                    tokio::time::timeout(
                        std::time::Duration::from_millis(50),
                        peer.read(&mut byte)
                    )
                    .await,
                    Ok(Ok(0))
                ),
                "oversized SCM_RIGHTS must not leak an installed FD at {index}"
            );
        }
    }
    #[tokio::test]
    async fn cleanup_drain_closes_pending_rights_and_requires_reliable_eof() {
        use std::os::fd::AsFd;
        let (mut sender, receiver) = tokio::net::UnixStream::pair().unwrap();
        let (mut flow, passed) = tokio::net::UnixStream::pair().unwrap();
        sender.write_all(b"partial header").await.unwrap();
        let raw = sender.as_fd().try_clone_to_owned().unwrap();
        crate::send_fd(&std::os::unix::net::UnixStream::from(raw), passed.as_fd()).unwrap();
        drop(passed);
        let drain = tokio::spawn(async move { drain_to_eof(&receiver).await });
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), flow.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(
            !drain.is_finished(),
            "discarded bytes are not cleanup acknowledgement"
        );
        drop(sender);
        drain.await.unwrap().unwrap();
    }
    #[test]
    fn valid_bind_decodes() {
        serde_json::from_str::<Request>(
            r#"{"version":1,"sequence":1,"epoch":7,"operation":{"kind":"bind"}}"#,
        )
        .unwrap();
    }
    #[tokio::test]
    async fn required_port_and_unknown_guest_identity_are_rejected() {
        for body in [
            r#"{"version":1,"sequence":1,"epoch":1,"operation":{"kind":"tcp","destination":"192.0.2.9"}}"#,
            r#"{"version":1,"sequence":1,"epoch":1,"operation":{"kind":"tcp","destination":"192.0.2.9","port":443,"session_id":"forged"}}"#,
        ] {
            let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
            bytes.extend_from_slice(body.as_bytes());
            assert!(read::<_, Request>(&mut bytes.as_slice()).await.is_err());
        }
    }
}
