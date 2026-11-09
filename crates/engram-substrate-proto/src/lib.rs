//! The substrate populate protocol (ADR 0075).
//!
//! One writer per host owns the NVMe chunk cache (the host-agent).
//! Read-only clients, the per-VM uffd handlers, ask it for chunks over
//! a UDS at `<work_dir>/substrate.sock`.
//!
//! Framing: 4-byte big-endian length prefix + bincode body. The client
//! side is synchronous `std::io`: the fault path is a blocking thread.
//!
//! Conversation, version 2:
//!
//! ```text
//!   handler ──[ Hello { proto_version: 2, .. } ]──► writer
//!   writer  ──[ HelloAck { .. } + SCM_RIGHTS slot fd ]──► handler
//!   handler ──[ Populate { hash } ]──► writer
//!   writer  ──[ Filled { len } ]──► handler      (bytes in slot[..len])
//!           or [ PopulateErr { msg } ]
//! ```
//!
//! The slot is a shared-memory file of [`SLOT_BYTES`] that the writer
//! creates for each connection. The writer copies the chunk into it and
//! the handler installs it straight from its mapping: one copy into
//! shared memory, no cache-file read. The exchange is strictly
//! request/response, so the writer writes the slot only while it serves
//! a request on that connection, and the handler reads it only between
//! a reply and its next request on that connection.
//!
//! Version 1 (served for one release, for handlers started by an older
//! host-agent; such handlers outlive a host-agent roll; #1772 removes it):
//!
//! ```text
//!   handler ──[ Hello { proto_version: 1, .. } ]──► writer   (optional)
//!   writer  ──[ HelloAck { .. } ]──► handler
//!   handler ──[ Populate { hash } ]──► writer
//!   writer  ──[ Populated { len } + SCM_RIGHTS fd ]──► handler
//!           or [ PopulateErr { msg } ]
//! ```
//!
//! A version-1 fd is opened `O_RDONLY` and holds the whole chunk from
//! offset 0. A connection without a `Hello` speaks version 1.
//!
//! Version 3 is version 2 plus [`ToWriter::Read`]: a request that says
//! who asks (guest memory or a disk) and may name a byte range of the
//! chunk. The reply is the same `Filled { len }`, with the range (or the
//! whole chunk) in `slot[..len]`. A writer refuses `Read` on a version-2
//! connection.
//!
//! A writer that does not speak the requested version replies
//! `PopulateErr` to the `Hello` and closes the connection. A writer
//! older than version 3 does the same to a version-3 `Hello`. A version-2
//! handler then dials again with version 1.

use std::io::{Read, Write};

use engram_core::types::manifest::ManifestRef;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// The version a current handler asks for first.
pub const PROTO_VERSION: u32 = 2;

/// The version that adds [`ToWriter::Read`]. Only a client that sends
/// `Read` asks for it.
pub const PROTO_VERSION_READ: u32 = 3;

/// The fd-per-reply version. A writer serves it to handlers that an
/// older host-agent started, and a handler falls back to it against an
/// older writer.
pub const PROTO_VERSION_V1: u32 = 1;

/// Frame size cap. Frames carry a hash or a manifest ref; chunk bytes
/// ride the slot or an fd, never a frame.
pub const MAX_MSG_BYTES: usize = 64 * 1024;

/// Size of the version-2 slot. The largest chunk a writer can deliver
/// through it; a larger chunk is a `PopulateErr`. The file is sparse,
/// so only the bytes of the largest chunk delivered so far use memory.
pub const SLOT_BYTES: u64 = 16 * 1024 * 1024;

/// Client → writer.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToWriter {
    /// Once per connection, first. Version 2 gets a slot fd after the
    /// ack.
    Hello {
        proto_version: u32,
        /// The base image manifest the handler faults against.
        canonical_manifest: Option<ManifestRef>,
        /// The session manifest the handler faults against. The writer
        /// pins its chunks in the cache while the connection is open.
        session_manifest: Option<ManifestRef>,
    },
    /// Make one chunk available and deliver it. Means guest memory and
    /// the whole chunk.
    Populate { hash: [u8; 32] },
    /// Version 3: deliver `range` of the chunk (the whole chunk when
    /// `None`) for `requester`.
    Read {
        hash: [u8; 32],
        requester: Requester,
        range: Option<ByteRange>,
    },
}

/// Who asks for a chunk. Positional on the wire: append only.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Requester {
    /// A uffd handler, for guest memory. The writer keeps the chunk in
    /// the cache.
    Memory,
    /// A session disk. A whole chunk is not kept in the cache: the
    /// volume holds it.
    Disk,
}

/// `len` bytes from `offset` of a chunk. A ranged read is not
/// hash-checked and does not go to the cache.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ByteRange {
    pub offset: u64,
    pub len: u64,
}

/// Writer → client. Variants are positional on the wire: append only.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum FromWriter {
    /// Live probe results. `tmpfs_ok` is a `statfs(TMPFS_MAGIC)` of the
    /// uffd base dir; a handler that gets `false` must exit, so the
    /// spawn fails instead of the VM failing later. `cache_writable` is
    /// a create+unlink probe of the cache root.
    HelloAck {
        tmpfs_ok: bool,
        cache_writable: bool,
    },
    /// Version 1: the chunk's `O_RDONLY` fd follows this frame as an
    /// SCM_RIGHTS message on a 1-byte carrier (see [`send_fd`]).
    Populated {
        len: u64,
    },
    PopulateErr {
        msg: String,
    },
    /// Version 2: the chunk is in `slot[..len]`.
    Filled {
        len: u64,
    },
}

/// The wire length of a frame body, from its 4-byte header.
pub fn frame_body_len(header: [u8; 4]) -> std::io::Result<usize> {
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_MSG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("substrate frame of {len} bytes exceeds cap {MAX_MSG_BYTES}"),
        ));
    }
    Ok(len)
}

/// Decode a frame body read after its header.
pub fn decode_frame_body<T: DeserializeOwned>(body: &[u8]) -> std::io::Result<T> {
    bincode::deserialize(body).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Encode `msg` as one frame: header and body.
pub fn encode_frame<T: Serialize>(msg: &T) -> std::io::Result<Vec<u8>> {
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
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Read one frame.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> std::io::Result<T> {
    let mut header = [0u8; 4];
    r.read_exact(&mut header)?;
    let mut body = vec![0u8; frame_body_len(header)?];
    r.read_exact(&mut body)?;
    decode_frame_body(&body)
}

/// Write one frame.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> std::io::Result<()> {
    w.write_all(&encode_frame(msg)?)?;
    w.flush()
}

/// Send `fd` over the stream socket `sock` via SCM_RIGHTS on a 1-byte
/// carrier. On a non-blocking socket this can fail with `WouldBlock`.
#[cfg(unix)]
pub fn send_fd(
    sock: std::os::fd::BorrowedFd<'_>,
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
#[cfg(unix)]
pub fn recv_fd(sock: std::os::fd::BorrowedFd<'_>) -> std::io::Result<std::os::fd::OwnedFd> {
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
    if msg.bytes == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "substrate socket closed before the fd",
        ));
    }
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
            ToWriter::Read {
                hash: [9u8; 32],
                requester: Requester::Disk,
                range: Some(ByteRange {
                    offset: 65_536,
                    len: 4096,
                }),
            },
            ToWriter::Read {
                hash: [9u8; 32],
                requester: Requester::Memory,
                range: None,
            },
        ];
        for m in msgs {
            let mut buf = Vec::new();
            write_frame(&mut buf, &m).unwrap();
            let back: ToWriter = read_frame(&mut buf.as_slice()).unwrap();
            assert_eq!(back, m);
        }
        for m in [
            FromWriter::Populated { len: 524_288 },
            FromWriter::Filled { len: 524_288 },
        ] {
            let mut buf = Vec::new();
            write_frame(&mut buf, &m).unwrap();
            let back: FromWriter = read_frame(&mut buf.as_slice()).unwrap();
            assert_eq!(back, m);
        }
    }

    /// A version-1 peer decodes these variants by position, so their
    /// encodings must not move.
    #[test]
    fn version_one_variants_keep_their_wire_positions() {
        let tag = |m: &FromWriter| bincode::serialize(m).unwrap()[..4].to_vec();
        assert_eq!(
            tag(&FromWriter::HelloAck {
                tmpfs_ok: true,
                cache_writable: true
            }),
            [0, 0, 0, 0]
        );
        assert_eq!(tag(&FromWriter::Populated { len: 1 }), [1, 0, 0, 0]);
        assert_eq!(
            tag(&FromWriter::PopulateErr { msg: String::new() }),
            [2, 0, 0, 0]
        );
        let to = |m: &ToWriter| bincode::serialize(m).unwrap()[..4].to_vec();
        assert_eq!(
            to(&ToWriter::Hello {
                proto_version: 1,
                canonical_manifest: None,
                session_manifest: None
            }),
            [0, 0, 0, 0]
        );
        assert_eq!(to(&ToWriter::Populate { hash: [0; 32] }), [1, 0, 0, 0]);
    }

    /// The version-3 read, byte for byte.
    #[test]
    fn a_read_has_a_fixed_wire_shape() {
        let read = ToWriter::Read {
            hash: [0xAB; 32],
            requester: Requester::Disk,
            range: Some(ByteRange {
                offset: 0x0102,
                len: 0x0304,
            }),
        };
        let mut want = vec![2, 0, 0, 0];
        want.extend_from_slice(&[0xAB; 32]);
        want.extend_from_slice(&[1, 0, 0, 0]);
        want.push(1);
        want.extend_from_slice(&0x0102u64.to_le_bytes());
        want.extend_from_slice(&0x0304u64.to_le_bytes());
        assert_eq!(bincode::serialize(&read).unwrap(), want);
        let whole = ToWriter::Read {
            hash: [0; 32],
            requester: Requester::Memory,
            range: None,
        };
        let bytes = bincode::serialize(&whole).unwrap();
        assert_eq!(bytes.len(), 4 + 32 + 4 + 1);
        assert_eq!(&bytes[36..], &[0, 0, 0, 0, 0]);
    }

    #[test]
    fn fd_round_trips_over_a_socketpair() {
        use std::io::{Read as _, Seek as _, Write as _};
        use std::os::fd::AsFd;
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(b"chunk bytes").unwrap();
        f.flush().unwrap();
        send_fd(a.as_fd(), f.as_fd()).unwrap();
        let received = recv_fd(b.as_fd()).unwrap();
        let mut file = std::fs::File::from(received);
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        let mut out = String::new();
        file.read_to_string(&mut out).unwrap();
        assert_eq!(out, "chunk bytes");
    }

    #[test]
    fn recv_fd_on_a_closed_socket_is_eof() {
        use std::os::fd::AsFd;
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(a);
        let err = recv_fd(b.as_fd()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }
}
