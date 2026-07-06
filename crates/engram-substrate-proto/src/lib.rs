//! ADR 0075: the substrate populate protocol.
//!
//! One writer per host owns the NVMe chunk cache (today the
//! host-agent; ADR 0076's `engram-substrated` inherits the socket
//! unchanged). Read-only clients — the per-VM uffd handlers — request
//! population over a UDS at `<work_dir>/substrate.sock` and receive
//! the verified chunk back as an `O_RDONLY` file descriptor via
//! SCM_RIGHTS: an eviction unlink after the reply is harmless because
//! the client reads a still-open fd.
//!
//! Framing: 4-byte big-endian length prefix + bincode body — the
//! `engram-migrate-proto` shape. Fully synchronous `std::io`; the
//! fault loop is a blocking thread and every dependency this crate
//! grows is bloat inside the page-fault path (`peer.rs` precedent:
//! "must never grow tonic/tokio").
//!
//! Conversation:
//!
//! ```text
//!   handler ──[ ToWriter::Hello { proto_version, canonical_manifest } ]──► writer
//!   writer  ──[ FromWriter::HelloAck { base_staged, tmpfs_ok } ]──► handler
//!     (sent once at handler startup, BEFORE the handler binds its
//!      FC-facing UDS — serving implies the writer confirmed staging)
//!
//!   handler ──[ ToWriter::Populate { hash } ]──► writer
//!   writer  ──[ FromWriter::Populated { len } + SCM_RIGHTS fd ]──► handler
//!           or [ FromWriter::PopulateErr { msg } ]
//! ```

use std::io::{Read, Write};

use engram_core::types::manifest::ManifestRef;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// Bump on any wire-incompatible change; the writer rejects
/// mismatches loudly in `HelloAck` position (a `PopulateErr`).
pub const PROTO_VERSION: u32 = 1;

/// Frame size cap. Messages are tiny (a hash, a manifest ref); the
/// chunk BYTES never ride the frame — they ride the fd.
pub const MAX_MSG_BYTES: usize = 64 * 1024;

/// Client → writer.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToWriter {
    /// Once, at handler startup. `canonical_manifest` names the base
    /// image manifest this handler will fault against, so the writer
    /// can answer `base_staged` from its `ImageReadiness`.
    Hello {
        proto_version: u32,
        canonical_manifest: Option<ManifestRef>,
        /// The session (divergence) manifest this handler faults
        /// against. The writer pins its chunk set for the CONNECTION
        /// lifetime — the handler holds the connection for the
        /// sandbox's lifetime, so pin lifetime == sandbox lifetime by
        /// construction, and the drop unpins (ADR 0075 phase 2: the
        /// property the old per-handler-cache doc claimed, restored in
        /// the only place pins now mean anything).
        session_manifest: Option<ManifestRef>,
    },
    /// Populate (or confirm resident) one chunk and return its fd.
    Populate { hash: [u8; 32] },
}

/// Writer → client.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum FromWriter {
    /// Probe results, not config echoes: `tmpfs_ok` is a live
    /// `statfs(TMPFS_MAGIC)` of the uffd base dir; `cache_writable`
    /// is a create+unlink probe of the cache root. A handler that
    /// receives `tmpfs_ok: false` must exit loudly so the spawn fails
    /// while the host-agent is provably alive — instead of a VM that
    /// boots and later dies with `register memory … userfaultfd …
    /// System error`. (`Hello.canonical_manifest` is carried for the
    /// ADR 0076 owner, whose readiness registry can answer
    /// staged-ness; the host-agent's registry keys on image digests,
    /// not manifest refs — divergence recorded in ADR 0075.)
    HelloAck {
        tmpfs_ok: bool,
        cache_writable: bool,
    },
    /// The chunk is resident and verified; the `O_RDONLY` fd follows
    /// this frame as an SCM_RIGHTS control message on a 1-byte
    /// carrier write (see [`send_fd`]/[`recv_fd`]).
    Populated {
        len: u64,
    },
    PopulateErr {
        msg: String,
    },
}

/// Read one length-prefixed bincode frame.
pub fn read_frame<R, T>(r: &mut R) -> std::io::Result<T>
where
    R: Read,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MSG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("substrate frame of {len} bytes exceeds cap {MAX_MSG_BYTES}"),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    bincode::deserialize(&buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Write one length-prefixed bincode frame.
pub fn write_frame<W, T>(w: &mut W, msg: &T) -> std::io::Result<()>
where
    W: Write,
    T: Serialize,
{
    let body = bincode::serialize(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if body.len() > MAX_MSG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "substrate frame of {} bytes exceeds cap {MAX_MSG_BYTES}",
                body.len()
            ),
        ));
    }
    w.write_all(&(body.len() as u32).to_be_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Send `fd` over `sock` via SCM_RIGHTS on a 1-byte carrier. Linux
/// only (the uffd handler is FC/Linux territory; VZ/Process backends
/// never spawn out-of-process cache clients — ADR 0075).
#[cfg(target_os = "linux")]
pub fn send_fd(
    sock: &std::os::unix::net::UnixStream,
    fd: std::os::fd::BorrowedFd<'_>,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let carrier = [0xEFu8];
    let iov = [std::io::IoSlice::new(&carrier)];
    let fds = [fd.as_raw_fd()];
    let cmsg = [nix::sys::socket::ControlMessage::ScmRights(&fds)];
    nix::sys::socket::sendmsg::<()>(
        sock.as_raw_fd(),
        &iov,
        &cmsg,
        nix::sys::socket::MsgFlags::empty(),
        None,
    )
    .map_err(std::io::Error::from)?;
    Ok(())
}

/// Receive one fd sent by [`send_fd`].
#[cfg(target_os = "linux")]
pub fn recv_fd(sock: &std::os::unix::net::UnixStream) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let mut carrier = [0u8; 1];
    let mut iov = [std::io::IoSliceMut::new(&mut carrier)];
    let mut cmsg_buf = nix::cmsg_space!([std::os::fd::RawFd; 1]);
    let msg = nix::sys::socket::recvmsg::<()>(
        sock.as_raw_fd(),
        &mut iov,
        Some(&mut cmsg_buf),
        nix::sys::socket::MsgFlags::empty(),
    )
    .map_err(std::io::Error::from)?;
    for c in msg.cmsgs().map_err(std::io::Error::from)? {
        if let nix::sys::socket::ControlMessageOwned::ScmRights(fds) = c {
            if let Some(&fd) = fds.first() {
                // SAFETY: the kernel just installed this fd into our
                // table via SCM_RIGHTS; we are its sole owner.
                return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "SCM_RIGHTS message carried no fd",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let msgs = vec![
            ToWriter::Hello {
                proto_version: PROTO_VERSION,
                canonical_manifest: None,
                session_manifest: None,
            },
            ToWriter::Populate { hash: [7u8; 32] },
        ];
        for m in msgs {
            let mut buf = Vec::new();
            write_frame(&mut buf, &m).unwrap();
            let back: ToWriter = read_frame(&mut buf.as_slice()).unwrap();
            assert_eq!(back, m);
        }
        let m = FromWriter::Populated { len: 524_288 };
        let mut buf = Vec::new();
        write_frame(&mut buf, &m).unwrap();
        let back: FromWriter = read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(back, m);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fd_round_trips_over_a_socketpair() {
        use std::io::{Read as _, Seek as _, Write as _};
        use std::os::fd::AsFd;
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(b"chunk bytes").unwrap();
        f.flush().unwrap();
        send_fd(&a, f.as_fd()).unwrap();
        let received = recv_fd(&b).unwrap();
        let mut file = std::fs::File::from(received);
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        let mut out = String::new();
        file.read_to_string(&mut out).unwrap();
        assert_eq!(out, "chunk bytes");
    }
}
