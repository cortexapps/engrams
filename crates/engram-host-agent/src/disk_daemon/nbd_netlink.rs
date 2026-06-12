//! Minimal generic-netlink client for the kernel NBD interface.
//!
//! Why netlink instead of the legacy `NBD_SET_SOCK`/`NBD_DO_IT`
//! ioctls: the ioctl mode runs the kernel's receive loop inside the
//! configuring process's thread (`NBD_DO_IT` blocks until
//! disconnect), so the device's data plane is structurally welded to
//! the host-agent process lifetime. A pod roll then kills the disk
//! under every surviving FC VM, and the dead binding can wedge the
//! slot until reboot ("device STILL bound after NBD_DISCONNECT +
//! NBD_CLEAR_SOCK", prod 2026-06-11, /dev/nbd4). The netlink mode
//! (kernel ≥ 4.18, designed by Facebook for exactly the
//! server-restart case) has no caller-blocked thread, supports
//! `NBD_ATTR_DEAD_CONN_TIMEOUT` (guest I/O QUEUES while the server
//! is gone instead of erroring), and `NBD_CMD_RECONFIGURE` (a new
//! process hands the kernel a fresh socket for a live device — the
//! survivor-rehydrate primitive).
//!
//! Hand-rolled over a raw `NETLINK_GENERIC` socket for the same
//! reason the FC client hand-rolls HTTP: the protocol surface we
//! need is tiny (one family resolve + three commands with a handful
//! of attributes) and not worth a netlink crate dependency tree.
//! Attribute ids verified against `<linux/nbd-netlink.h>`.

use std::io;
use std::os::fd::RawFd;
use std::sync::OnceLock;

// ---- netlink core ---------------------------------------------------

const NETLINK_GENERIC: libc::c_int = 16;
const NLM_F_REQUEST: u16 = 0x01;
const NLM_F_ACK: u16 = 0x04;
const NLMSG_ERROR: u16 = 0x2;
/// Attribute header size; payloads align to 4.
const NLA_HDRLEN: usize = 4;
const NLA_F_NESTED: u16 = 0x8000;

// genetlink controller (family id resolution).
const GENL_ID_CTRL: u16 = 0x10;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;

// ---- NBD genetlink family (uapi/linux/nbd-netlink.h) ----------------

const NBD_GENL_FAMILY_NAME: &str = "nbd";
const NBD_GENL_VERSION: u8 = 1;

const NBD_CMD_CONNECT: u8 = 1;
const NBD_CMD_DISCONNECT: u8 = 2;
const NBD_CMD_RECONFIGURE: u8 = 3;

const NBD_ATTR_INDEX: u16 = 1; // u32
const NBD_ATTR_SIZE_BYTES: u16 = 2; // u64
const NBD_ATTR_BLOCK_SIZE_BYTES: u16 = 3; // u64
const NBD_ATTR_TIMEOUT: u16 = 4; // u64 (seconds)
const NBD_ATTR_SERVER_FLAGS: u16 = 5; // u64
const NBD_ATTR_SOCKETS: u16 = 7; // nested list
const NBD_ATTR_DEAD_CONN_TIMEOUT: u16 = 8; // u64 (seconds)
const NBD_ATTR_BACKEND_IDENTIFIER: u16 = 10; // NUL-terminated string

const NBD_SOCK_ITEM: u16 = 1; // nested
const NBD_SOCK_FD: u16 = 1; // u32

/// Connection parameters shared by [`connect_device`] and
/// [`reconfigure_device`].
pub struct NbdNetlinkParams<'a> {
    /// Device minor, i.e. the `N` of `/dev/nbdN`.
    pub index: u32,
    /// Kernel-side half of the serve socketpair. The kernel dups it.
    pub sock_fd: RawFd,
    /// Per-request timeout (seconds) while a connection is live.
    pub timeout_secs: u64,
    /// How long (seconds) queued I/O survives with NO live
    /// connection before failing — the pod-roll grace window. The
    /// kernel requeues timed-out requests instead of erroring while
    /// this window is open, so a guest rides out a host-agent
    /// restart in D-state rather than taking EIO.
    pub dead_conn_timeout_secs: u64,
    /// Stable identity tag the kernel stores on CONNECT and verifies
    /// on RECONFIGURE (`/sys/block/nbdN/backend`). We use the disk
    /// manifest id: stable for the attach lifetime and known to both
    /// the original attach and the survivor rehydrate.
    pub backend_identifier: &'a str,
}

/// Build one netlink attribute: u16 len, u16 type, payload, 4-pad.
fn put_attr(buf: &mut Vec<u8>, ty: u16, payload: &[u8]) {
    let len = (NLA_HDRLEN + payload.len()) as u16;
    buf.extend_from_slice(&len.to_ne_bytes());
    buf.extend_from_slice(&ty.to_ne_bytes());
    buf.extend_from_slice(payload);
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}

fn put_attr_u32(buf: &mut Vec<u8>, ty: u16, v: u32) {
    put_attr(buf, ty, &v.to_ne_bytes());
}

fn put_attr_u64(buf: &mut Vec<u8>, ty: u16, v: u64) {
    put_attr(buf, ty, &v.to_ne_bytes());
}

/// Nested attribute: emit a placeholder header, fill children via
/// `f`, then back-patch the length.
fn put_attr_nested(buf: &mut Vec<u8>, ty: u16, f: impl FnOnce(&mut Vec<u8>)) {
    let start = buf.len();
    buf.extend_from_slice(&0u16.to_ne_bytes());
    buf.extend_from_slice(&(ty | NLA_F_NESTED).to_ne_bytes());
    f(buf);
    let len = (buf.len() - start) as u16;
    buf[start..start + 2].copy_from_slice(&len.to_ne_bytes());
    // Children are individually padded; the nest needs no tail pad
    // beyond that, but keep the invariant explicit.
    debug_assert_eq!(buf.len() % 4, 0);
}

/// Frame a full genetlink request: nlmsghdr + genlmsghdr + attrs.
fn genl_message(family: u16, cmd: u8, version: u8, attrs: &[u8]) -> Vec<u8> {
    const NLMSG_HDRLEN: usize = 16;
    const GENL_HDRLEN: usize = 4;
    let total = NLMSG_HDRLEN + GENL_HDRLEN + attrs.len();
    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(&(total as u32).to_ne_bytes()); // nlmsg_len
    buf.extend_from_slice(&family.to_ne_bytes()); // nlmsg_type
    buf.extend_from_slice(&(NLM_F_REQUEST | NLM_F_ACK).to_ne_bytes()); // nlmsg_flags
    buf.extend_from_slice(&1u32.to_ne_bytes()); // nlmsg_seq
    buf.extend_from_slice(&0u32.to_ne_bytes()); // nlmsg_pid (kernel fills)
    buf.push(cmd); // genl cmd
    buf.push(version); // genl version
    buf.extend_from_slice(&0u16.to_ne_bytes()); // genl reserved
    buf.extend_from_slice(attrs);
    buf
}

/// One blocking request/response round-trip on a fresh
/// `NETLINK_GENERIC` socket. Returns the raw response bytes.
///
/// A fresh socket per call keeps this free of shared mutable state —
/// these calls happen a handful of times per sandbox lifetime, not
/// on the data path.
fn genl_roundtrip(msg: &[u8]) -> io::Result<Vec<u8>> {
    // SAFETY: plain socket(2); fd ownership handed to OwnedFd below.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            NETLINK_GENERIC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);

    // Bound the wait so a wedged netlink path can't hang a Drop or a
    // startup pass. 10s is far above any observed genl latency.
    let tv = libc::timeval {
        tv_sec: 10,
        tv_usec: 0,
    };
    // SAFETY: raw is a valid owned socket; timeval is a stack value.
    unsafe {
        libc::setsockopt(
            raw,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            (&tv as *const libc::timeval).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
    }

    // SAFETY: msg is a valid initialized buffer.
    let sent = unsafe { libc::send(raw, msg.as_ptr().cast(), msg.len(), 0) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut resp = vec![0u8; 8192];
    // SAFETY: resp is a valid writable buffer of the stated length.
    let n = unsafe { libc::recv(raw, resp.as_mut_ptr().cast(), resp.len(), 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    resp.truncate(n as usize);
    Ok(resp)
}

/// Interpret a netlink response: an `NLMSG_ERROR` with error 0 is
/// the ACK; a negative value is `-errno`. Any other message type is
/// handed back for command-specific parsing.
fn check_ack(resp: &[u8]) -> io::Result<Option<&[u8]>> {
    if resp.len() < 16 {
        return Err(io::Error::other("netlink response shorter than nlmsghdr"));
    }
    let ty = u16::from_ne_bytes([resp[4], resp[5]]);
    if ty == NLMSG_ERROR {
        if resp.len() < 20 {
            return Err(io::Error::other("NLMSG_ERROR shorter than its payload"));
        }
        let err = i32::from_ne_bytes([resp[16], resp[17], resp[18], resp[19]]);
        if err == 0 {
            return Ok(None); // clean ACK
        }
        return Err(io::Error::from_raw_os_error(-err));
    }
    Ok(Some(resp))
}

/// Resolve (and cache) the NBD genetlink family id. The id is
/// kernel-boot-stable, so one successful resolve serves the process
/// lifetime.
fn nbd_family_id() -> io::Result<u16> {
    static FAMILY: OnceLock<u16> = OnceLock::new();
    if let Some(id) = FAMILY.get() {
        return Ok(*id);
    }
    let mut attrs = Vec::new();
    let mut name = NBD_GENL_FAMILY_NAME.as_bytes().to_vec();
    name.push(0);
    put_attr(&mut attrs, CTRL_ATTR_FAMILY_NAME, &name);
    let msg = genl_message(GENL_ID_CTRL, CTRL_CMD_GETFAMILY, 2, &attrs);
    let resp = genl_roundtrip(&msg)?;
    let body = check_ack(&resp)?.ok_or_else(|| {
        io::Error::other("CTRL_CMD_GETFAMILY returned a bare ACK with no family payload")
    })?;
    // Walk the response attrs (skip nlmsghdr 16 + genlmsghdr 4).
    let mut off = 20;
    while off + NLA_HDRLEN <= body.len() {
        let len = u16::from_ne_bytes([body[off], body[off + 1]]) as usize;
        let ty = u16::from_ne_bytes([body[off + 2], body[off + 3]]) & !NLA_F_NESTED;
        if len < NLA_HDRLEN || off + len > body.len() {
            break;
        }
        if ty == CTRL_ATTR_FAMILY_ID && len >= NLA_HDRLEN + 2 {
            let id = u16::from_ne_bytes([body[off + 4], body[off + 5]]);
            return Ok(*FAMILY.get_or_init(|| id));
        }
        off += (len + 3) & !3;
    }
    Err(io::Error::other(
        "nbd genetlink family not found (nbd.ko not loaded?)",
    ))
}

/// Shared attr block for CONNECT/RECONFIGURE.
fn conn_attrs(p: &NbdNetlinkParams<'_>, include_geometry: Option<(u64, u64)>) -> Vec<u8> {
    let mut attrs = Vec::new();
    put_attr_u32(&mut attrs, NBD_ATTR_INDEX, p.index);
    if let Some((size_bytes, block_size)) = include_geometry {
        put_attr_u64(&mut attrs, NBD_ATTR_SIZE_BYTES, size_bytes);
        put_attr_u64(&mut attrs, NBD_ATTR_BLOCK_SIZE_BYTES, block_size);
        put_attr_u64(&mut attrs, NBD_ATTR_SERVER_FLAGS, p_server_flags());
    }
    put_attr_u64(&mut attrs, NBD_ATTR_TIMEOUT, p.timeout_secs);
    put_attr_u64(
        &mut attrs,
        NBD_ATTR_DEAD_CONN_TIMEOUT,
        p.dead_conn_timeout_secs,
    );
    let mut ident = p.backend_identifier.as_bytes().to_vec();
    ident.push(0);
    put_attr(&mut attrs, NBD_ATTR_BACKEND_IDENTIFIER, &ident);
    put_attr_nested(&mut attrs, NBD_ATTR_SOCKETS, |buf| {
        put_attr_nested(buf, NBD_SOCK_ITEM, |buf| {
            put_attr_u32(buf, NBD_SOCK_FD, p.sock_fd as u32);
        });
    });
    attrs
}

/// `NBD_FLAG_HAS_FLAGS | NBD_FLAG_SEND_FLUSH | NBD_FLAG_SEND_TRIM` —
/// same capability bits the legacy ioctl attach advertised, so guest
/// fsync → `NBD_CMD_FLUSH` and fstrim → `NBD_CMD_TRIM` keep flowing.
fn p_server_flags() -> u64 {
    ((1u32 << 0) | (1 << 2) | (1 << 5)) as u64
}

/// `NBD_CMD_CONNECT`: configure `/dev/nbd<index>` to serve from
/// `sock_fd`. Fails with `EBUSY` if the device already has a config
/// — which is exactly the signal we want when slot accounting goes
/// wrong.
pub fn connect_device(
    p: &NbdNetlinkParams<'_>,
    size_bytes: u64,
    block_size: u64,
) -> io::Result<()> {
    let family = nbd_family_id()?;
    let attrs = conn_attrs(p, Some((size_bytes, block_size)));
    let msg = genl_message(family, NBD_CMD_CONNECT, NBD_GENL_VERSION, &attrs);
    check_ack(&genl_roundtrip(&msg)?)?;
    Ok(())
}

/// `NBD_CMD_RECONFIGURE`: hand a NEW serve socket to an
/// already-configured device whose previous connection died — the
/// survivor-rehydrate primitive. The kernel replaces the dead
/// connection slot and requeues any I/O parked under
/// `dead_conn_timeout`. Requires the device to have been configured
/// via netlink with a matching `backend_identifier`.
pub fn reconfigure_device(p: &NbdNetlinkParams<'_>) -> io::Result<()> {
    let family = nbd_family_id()?;
    let attrs = conn_attrs(p, None);
    let msg = genl_message(family, NBD_CMD_RECONFIGURE, NBD_GENL_VERSION, &attrs);
    check_ack(&genl_roundtrip(&msg)?)?;
    Ok(())
}

/// `NBD_CMD_DISCONNECT`: tear the device's config down. Unlike the
/// legacy ioctl path this needs no open fd and no blocked thread —
/// it works on a device whose configuring process is long dead.
pub fn disconnect_device(index: u32) -> io::Result<()> {
    let family = nbd_family_id()?;
    let mut attrs = Vec::new();
    put_attr_u32(&mut attrs, NBD_ATTR_INDEX, index);
    let msg = genl_message(family, NBD_CMD_DISCONNECT, NBD_GENL_VERSION, &attrs);
    check_ack(&genl_roundtrip(&msg)?)?;
    Ok(())
}

/// Parse the device minor out of a `/dev/nbdN` path.
pub fn device_index(path: &std::path::Path) -> io::Result<u32> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    name.strip_prefix("nbd")
        .and_then(|n| n.parse::<u32>().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not an nbd device path: {}", path.display()),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attr_encoding_is_4_aligned_with_native_endian_headers() {
        let mut buf = Vec::new();
        put_attr_u32(&mut buf, NBD_ATTR_INDEX, 4);
        // len = 4 (hdr) + 4 (payload) = 8, type = 1, payload LE/native.
        assert_eq!(buf.len(), 8);
        assert_eq!(u16::from_ne_bytes([buf[0], buf[1]]), 8);
        assert_eq!(u16::from_ne_bytes([buf[2], buf[3]]), NBD_ATTR_INDEX);
        assert_eq!(u32::from_ne_bytes([buf[4], buf[5], buf[6], buf[7]]), 4);

        // Odd-length string payload pads to 4.
        let mut buf = Vec::new();
        put_attr(&mut buf, NBD_ATTR_BACKEND_IDENTIFIER, b"abc\0");
        assert_eq!(buf.len(), 8);
        let mut buf = Vec::new();
        put_attr(&mut buf, NBD_ATTR_BACKEND_IDENTIFIER, b"abcd\0");
        assert_eq!(buf.len(), 12, "5-byte payload pads to the next 4-boundary");
        // Declared length excludes the pad.
        assert_eq!(u16::from_ne_bytes([buf[0], buf[1]]), 9);
    }

    #[test]
    fn nested_sockets_attr_shape_matches_uapi_layout() {
        // [NBD_ATTR_SOCKETS [NBD_SOCK_ITEM [NBD_SOCK_FD u32]]]
        let mut buf = Vec::new();
        put_attr_nested(&mut buf, NBD_ATTR_SOCKETS, |buf| {
            put_attr_nested(buf, NBD_SOCK_ITEM, |buf| {
                put_attr_u32(buf, NBD_SOCK_FD, 42);
            });
        });
        // 4 (outer hdr) + 4 (item hdr) + 8 (fd attr) = 16.
        assert_eq!(buf.len(), 16);
        assert_eq!(u16::from_ne_bytes([buf[0], buf[1]]), 16);
        assert_eq!(
            u16::from_ne_bytes([buf[2], buf[3]]),
            NBD_ATTR_SOCKETS | NLA_F_NESTED
        );
        assert_eq!(u16::from_ne_bytes([buf[4], buf[5]]), 12);
        assert_eq!(
            u16::from_ne_bytes([buf[6], buf[7]]),
            NBD_SOCK_ITEM | NLA_F_NESTED
        );
        assert_eq!(u32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]), 42);
    }

    #[test]
    fn genl_message_frames_nlmsghdr_plus_genlmsghdr() {
        let msg = genl_message(0x1e, NBD_CMD_CONNECT, NBD_GENL_VERSION, &[1, 2, 3, 4]);
        assert_eq!(msg.len(), 24);
        assert_eq!(u32::from_ne_bytes([msg[0], msg[1], msg[2], msg[3]]), 24); // nlmsg_len
        assert_eq!(u16::from_ne_bytes([msg[4], msg[5]]), 0x1e); // family
        assert_eq!(
            u16::from_ne_bytes([msg[6], msg[7]]),
            NLM_F_REQUEST | NLM_F_ACK
        );
        assert_eq!(msg[16], NBD_CMD_CONNECT);
        assert_eq!(msg[17], NBD_GENL_VERSION);
    }

    #[test]
    fn device_index_parses_minor() {
        assert_eq!(device_index(std::path::Path::new("/dev/nbd0")).unwrap(), 0);
        assert_eq!(
            device_index(std::path::Path::new("/dev/nbd17")).unwrap(),
            17
        );
        assert!(device_index(std::path::Path::new("/dev/sda")).is_err());
    }

    #[test]
    fn nlmsg_error_payload_maps_to_errno() {
        // Hand-built NLMSG_ERROR carrying -EBUSY.
        let mut resp = Vec::new();
        resp.extend_from_slice(&20u32.to_ne_bytes());
        resp.extend_from_slice(&NLMSG_ERROR.to_ne_bytes());
        resp.extend_from_slice(&0u16.to_ne_bytes());
        resp.extend_from_slice(&1u32.to_ne_bytes());
        resp.extend_from_slice(&0u32.to_ne_bytes());
        resp.extend_from_slice(&(-libc::EBUSY).to_ne_bytes());
        let err = check_ack(&resp).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EBUSY));

        // Error 0 = ACK.
        let mut ack = resp.clone();
        ack[16..20].copy_from_slice(&0i32.to_ne_bytes());
        assert!(check_ack(&ack).unwrap().is_none());
    }
}
