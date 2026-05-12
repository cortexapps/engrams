//! NBD wire-format codec.
//!
//! Per the kernel-side NBD transmission protocol (the bit that
//! follows handshake — when we use `NBD_SET_SOCK` ioctl mode, the
//! kernel handles the handshake itself and our daemon only sees
//! the transmission phase).
//!
//! - **Request** (28 bytes, big-endian):
//!   - `magic`  : u32 = `NBD_REQUEST_MAGIC` (0x25609513)
//!   - `flags`  : u16
//!   - `type`   : u16 (NBD command)
//!   - `handle` : u64 (opaque to server; echoed verbatim in reply)
//!   - `offset` : u64
//!   - `length` : u32
//!   - For `NBD_CMD_WRITE`, `length` data bytes follow the header.
//!
//! - **Reply** (16 bytes, big-endian):
//!   - `magic`  : u32 = `NBD_REPLY_MAGIC` (0x67446698)
//!   - `error`  : u32 (0 = success; otherwise errno-style)
//!   - `handle` : u64 (echo from request)
//!   - For `NBD_CMD_READ`, `length` data bytes follow the header.
//!
//! References:
//! - <https://github.com/NetworkBlockDevice/nbd/blob/master/doc/proto.md>
//! - `<linux/nbd.h>` for the canonical magic numbers and command codes.
//!
//! This module is target-agnostic so macOS dev can lock the codec
//! down with unit tests before the Linux-only server loop wires it
//! to a real kernel `/dev/nbdN` endpoint.

use std::io::{self, ErrorKind};

/// `NBD_REQUEST_MAGIC` from `<linux/nbd.h>`. Every request the
/// kernel side sends begins with this u32 in network byte order.
pub const NBD_REQUEST_MAGIC: u32 = 0x2560_9513;

/// `NBD_REPLY_MAGIC` from `<linux/nbd.h>`. The daemon prefixes every
/// reply with this; the kernel rejects anything else as a protocol
/// error.
pub const NBD_REPLY_MAGIC: u32 = 0x6744_6698;

/// Fixed sizes for the two structured headers. Useful for buffer
/// pre-sizing; not directly exposed to callers.
pub const REQUEST_HEADER_LEN: usize = 28;
pub const REPLY_HEADER_LEN: usize = 16;

/// NBD command types (the `type` u16 in the request header).
///
/// We deliberately keep this enum small — the daemon supports the
/// transmission-phase commands the kernel actually uses for a
/// virtio-blk-attached `/dev/nbdN`. The "structured reply" /
/// extended-header variants (NBD_CMD_BLOCK_STATUS, etc.) only show
/// up if we opt into structured replies during handshake; we don't.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NbdCommand {
    /// `NBD_CMD_READ` (0). Server returns `length` bytes from
    /// offset.
    Read,
    /// `NBD_CMD_WRITE` (1). Server reads `length` bytes after the
    /// header and persists them.
    Write,
    /// `NBD_CMD_DISC` (2). Client wants to disconnect; server should
    /// finish in-flight requests and close.
    Disconnect,
    /// `NBD_CMD_FLUSH` (3). Server should commit pending writes
    /// (offset + length are unused). The chunked daemon treats this
    /// as a hint — flush-on-snapshot is the real durability gate,
    /// not flush-on-every-FLUSH-request.
    Flush,
    /// `NBD_CMD_TRIM` (4). Discard / punch-hole the range. The
    /// chunked daemon treats this as a write-of-zeros (kernel
    /// behaviour-equivalent for a virtio-blk backed by a regular
    /// file).
    Trim,
}

impl NbdCommand {
    fn from_u16(t: u16) -> Result<Self, NbdWireError> {
        match t {
            0 => Ok(NbdCommand::Read),
            1 => Ok(NbdCommand::Write),
            2 => Ok(NbdCommand::Disconnect),
            3 => Ok(NbdCommand::Flush),
            4 => Ok(NbdCommand::Trim),
            other => Err(NbdWireError::UnknownCommand(other)),
        }
    }
}

/// Parsed transmission-phase NBD request header (no payload).
/// The `Write` data is not part of this struct — callers read it
/// separately because the read width depends on `length`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NbdRequest {
    /// Request type. See [`NbdCommand`].
    pub command: NbdCommand,
    /// Request flags. `NBD_CMD_FLAG_FUA` (1 << 0) is the only flag
    /// the chunked daemon recognises today — and even then, treats
    /// as advisory because dirty-chunk persistence is snapshot-
    /// triggered.
    pub flags: u16,
    /// Opaque request id. Echoed in the reply header so the kernel
    /// can correlate; the daemon never inspects this.
    pub handle: u64,
    /// Byte offset into the virtual disk where the operation lands.
    pub offset: u64,
    /// Byte length. For READ this is the response payload size;
    /// for WRITE it's the payload size the client streams after the
    /// header.
    pub length: u32,
}

impl NbdRequest {
    /// Parse a 28-byte request header. Returns `Err` if the magic
    /// doesn't match — the kernel side is expected to be
    /// well-behaved here, so a magic mismatch indicates a protocol
    /// fault (corrupted socket / wrong endpoint).
    pub fn parse(buf: &[u8; REQUEST_HEADER_LEN]) -> Result<Self, NbdWireError> {
        let magic = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != NBD_REQUEST_MAGIC {
            return Err(NbdWireError::BadMagic {
                got: magic,
                expected: NBD_REQUEST_MAGIC,
            });
        }
        let flags = u16::from_be_bytes([buf[4], buf[5]]);
        let cmd_type = u16::from_be_bytes([buf[6], buf[7]]);
        let handle = u64::from_be_bytes([
            buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
        ]);
        let offset = u64::from_be_bytes([
            buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23],
        ]);
        let length = u32::from_be_bytes([buf[24], buf[25], buf[26], buf[27]]);
        Ok(Self {
            command: NbdCommand::from_u16(cmd_type)?,
            flags,
            handle,
            offset,
            length,
        })
    }
}

/// Transmission-phase NBD reply header.
///
/// The daemon constructs one of these per request; for READ replies
/// the response data follows immediately after the encoded header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NbdReply {
    /// 0 on success; otherwise a Linux errno-style code the kernel
    /// surfaces back to the in-VM caller. The chunked daemon maps
    /// chunk-store I/O failures to `EIO` (5) and out-of-range
    /// offsets to `EINVAL` (22).
    pub error: u32,
    /// Echo of the request's handle. Required for the kernel to
    /// correlate this reply with an in-flight request — the kernel
    /// allows many concurrent in-flight commands per socket.
    pub handle: u64,
}

impl NbdReply {
    /// Encode the 16-byte header into a fixed buffer. The caller
    /// writes the buffer followed by any payload (`length` bytes
    /// of data for READ responses, nothing for WRITE / FLUSH /
    /// DISC / TRIM responses).
    pub fn encode(&self) -> [u8; REPLY_HEADER_LEN] {
        let mut out = [0u8; REPLY_HEADER_LEN];
        out[0..4].copy_from_slice(&NBD_REPLY_MAGIC.to_be_bytes());
        out[4..8].copy_from_slice(&self.error.to_be_bytes());
        out[8..16].copy_from_slice(&self.handle.to_be_bytes());
        out
    }

    /// Convenience helper for the common "successful response" case.
    pub fn ok(handle: u64) -> Self {
        Self { error: 0, handle }
    }

    /// EIO / "I/O error" reply. Use when chunk-store reads fail or
    /// other transient backend errors land.
    pub fn eio(handle: u64) -> Self {
        Self { error: 5, handle }
    }

    /// EINVAL / "invalid argument" reply. Use for out-of-range
    /// offsets or unrecognised commands.
    pub fn einval(handle: u64) -> Self {
        Self { error: 22, handle }
    }
}

/// Anything that can go wrong parsing NBD wire bytes.
#[derive(Debug)]
pub enum NbdWireError {
    BadMagic { got: u32, expected: u32 },
    UnknownCommand(u16),
}

impl std::fmt::Display for NbdWireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic { got, expected } => write!(
                f,
                "NBD wire magic mismatch: got {got:#x}, expected {expected:#x}"
            ),
            Self::UnknownCommand(t) => write!(f, "unknown NBD command type: {t}"),
        }
    }
}

impl std::error::Error for NbdWireError {}

impl From<NbdWireError> for io::Error {
    fn from(e: NbdWireError) -> Self {
        io::Error::new(ErrorKind::InvalidData, e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthesize a request header for the given fields, encoded
    /// big-endian. Mirrors the kernel's serialization so tests
    /// can exercise the parser against bit-level-accurate bytes.
    fn make_request_bytes(
        cmd: u16,
        flags: u16,
        handle: u64,
        offset: u64,
        length: u32,
    ) -> [u8; REQUEST_HEADER_LEN] {
        let mut buf = [0u8; REQUEST_HEADER_LEN];
        buf[0..4].copy_from_slice(&NBD_REQUEST_MAGIC.to_be_bytes());
        buf[4..6].copy_from_slice(&flags.to_be_bytes());
        buf[6..8].copy_from_slice(&cmd.to_be_bytes());
        buf[8..16].copy_from_slice(&handle.to_be_bytes());
        buf[16..24].copy_from_slice(&offset.to_be_bytes());
        buf[24..28].copy_from_slice(&length.to_be_bytes());
        buf
    }

    #[test]
    fn parses_read_request_byte_for_byte() {
        let bytes = make_request_bytes(0, 0, 0xdead_beef_cafe_babe, 4096, 8192);
        let req = NbdRequest::parse(&bytes).unwrap();
        assert_eq!(req.command, NbdCommand::Read);
        assert_eq!(req.flags, 0);
        assert_eq!(req.handle, 0xdead_beef_cafe_babe);
        assert_eq!(req.offset, 4096);
        assert_eq!(req.length, 8192);
    }

    #[test]
    fn parses_write_request_with_fua_flag() {
        // NBD_CMD_FLAG_FUA = 1, NBD_CMD_WRITE = 1.
        let bytes = make_request_bytes(1, 1, 0x1234, 1 << 20, 64 * 1024);
        let req = NbdRequest::parse(&bytes).unwrap();
        assert_eq!(req.command, NbdCommand::Write);
        assert_eq!(req.flags, 1);
        assert_eq!(req.offset, 1 << 20);
        assert_eq!(req.length, 64 * 1024);
    }

    #[test]
    fn parses_each_known_command_variant() {
        for (code, expected) in [
            (0, NbdCommand::Read),
            (1, NbdCommand::Write),
            (2, NbdCommand::Disconnect),
            (3, NbdCommand::Flush),
            (4, NbdCommand::Trim),
        ] {
            let bytes = make_request_bytes(code, 0, 0, 0, 0);
            let req = NbdRequest::parse(&bytes).unwrap();
            assert_eq!(req.command, expected, "command code {code}");
        }
    }

    #[test]
    fn rejects_bad_magic_loud() {
        let mut bytes = make_request_bytes(0, 0, 1, 0, 0);
        // Flip the first byte so the magic decodes to something
        // other than NBD_REQUEST_MAGIC. The kernel side never
        // sends a misaligned magic on a healthy socket — a magic
        // mismatch means the socket itself is wrong.
        bytes[0] ^= 0xff;
        let err = NbdRequest::parse(&bytes).unwrap_err();
        match err {
            NbdWireError::BadMagic { expected, .. } => {
                assert_eq!(expected, NBD_REQUEST_MAGIC);
            }
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_command_type() {
        // 0x99 isn't a defined NBD command. The daemon refuses
        // rather than guessing; the kernel-side test path would
        // not realistically reach this branch but a corrupt socket
        // might.
        let bytes = make_request_bytes(0x99, 0, 0, 0, 0);
        let err = NbdRequest::parse(&bytes).unwrap_err();
        assert!(matches!(err, NbdWireError::UnknownCommand(0x99)));
    }

    #[test]
    fn reply_encodes_success_with_correct_magic_and_handle() {
        let reply = NbdReply::ok(0xabcd_ef01);
        let bytes = reply.encode();
        let magic = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let error = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let handle = u64::from_be_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]);
        assert_eq!(magic, NBD_REPLY_MAGIC);
        assert_eq!(error, 0);
        assert_eq!(handle, 0xabcd_ef01);
    }

    #[test]
    fn reply_encodes_error_codes_canonically() {
        assert_eq!(
            u32::from_be_bytes(NbdReply::eio(0).encode()[4..8].try_into().unwrap()),
            5
        );
        assert_eq!(
            u32::from_be_bytes(NbdReply::einval(0).encode()[4..8].try_into().unwrap()),
            22
        );
    }

    #[test]
    fn reply_encode_round_trips_through_byte_buffer() {
        // Sanity: a reply built then parsed by an independent
        // decoder must recover the same logical values. Guards
        // against accidental endianness flips during refactor.
        let r = NbdReply {
            error: 42,
            handle: 0x12_34_56_78_9a_bc_de_f0,
        };
        let bytes = r.encode();
        let magic = u32::from_be_bytes(bytes[0..4].try_into().unwrap());
        let error = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
        let handle = u64::from_be_bytes(bytes[8..16].try_into().unwrap());
        assert_eq!(magic, NBD_REPLY_MAGIC);
        assert_eq!(error, 42);
        assert_eq!(handle, 0x12_34_56_78_9a_bc_de_f0);
    }
}
