//! ADR 0121: the control protocol between host-agent and the
//! node-local egress daemon (`engram-egress-proxyd`).
//!
//! The daemon owns the egress listeners and outlives host-agent pods,
//! so in-flight guest streams survive a pod roll. Host-agent is the
//! control plane: it spawns or adopts the daemon (see [`adopt`]),
//! then applies session egress policies over a UDS at
//! `<work_dir>/egress-proxyd.sock`.
//!
//! Framing: 4-byte big-endian length prefix + a JSON body. JSON, not
//! bincode, on purpose: the policy types carry `#[serde(default)]`
//! evolution and the two ends of this socket can be different builds
//! (the successor pod talks to a daemon an older pod spawned), so the
//! wire must be self-describing. `Hello`/`HelloAck` must decode across
//! versions for the fingerprint compare to run at all.
//!
//! Conversation (control socket):
//!
//! ```text
//!   host-agent ──[ ToProxyd::Hello { proto_version } ]──► proxyd
//!   proxyd     ──[ FromProxyd::HelloAck { .. } ]──► host-agent
//!     (first frame on every connection; proxyd answers from its
//!      spawn-time config, so the successor can compare fingerprints
//!      and config without trusting the manifest file alone)
//!
//!   host-agent ──[ ToProxyd::SyncPolicies(all) ]──► proxyd   (on connect)
//!   host-agent ──[ ToProxyd::ApplyPolicy(one) ]──► proxyd    (live apply)
//!   host-agent ──[ ToProxyd::RemoveSession(id) ]──► proxyd   (destroy)
//!   proxyd     ──[ FromProxyd::Ok | FromProxyd::Err(..) ]──► host-agent
//! ```
//!
//! The dial-back socket (`<work_dir>/egress-dialback.sock`) runs the
//! opposite direction: proxyd asks host-agent to open an ADR 0118
//! guest stream, and the connected fd comes back via SCM_RIGHTS on a
//! 1-byte carrier (the `engram-substrate-proto` mechanism).

pub mod adopt;
pub mod manifest;

use engram_core::types::egress::SessionEgressPolicy;
use engram_core::{SandboxId, SessionId};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Bump on any wire-incompatible change. A mismatch in `Hello`
/// position is never adopted — the caller kills and respawns.
pub const PROTO_VERSION: u32 = 1;

/// Frame size cap. `SyncPolicies` carries every live session's policy
/// (resolved secrets included), so the cap is generous; both ends are
/// root-local so this bounds accidents, not adversaries.
pub const MAX_MSG_BYTES: usize = 64 * 1024 * 1024;

/// The control socket, relative to the work dir.
pub const CONTROL_SOCK_NAME: &str = "egress-proxyd.sock";
/// The daemon's manifest, relative to the work dir.
pub const MANIFEST_NAME: &str = "egress-proxyd.json";
/// The ADR 0118 dial-back socket, relative to the work dir.
pub const DIALBACK_SOCK_NAME: &str = "egress-dialback.sock";
/// The daemon's log file, relative to the work dir. Pod-log capture
/// dies with the pod; the daemon's diagnostics must not.
pub const LOG_FILE_NAME: &str = "egress-proxyd.log";

/// Spawn-time env for the daemon: CA cert PEM (the `EnvCaSource`
/// pattern — argv leaks on `/proc/*/cmdline`, env does not).
pub const ENV_CA_CERT_PEM: &str = "ENGRAM_EGRESS_PROXYD_CA_CERT_PEM";
/// Spawn-time env for the daemon: CA key PEM.
pub const ENV_CA_KEY_PEM: &str = "ENGRAM_EGRESS_PROXYD_CA_KEY_PEM";
/// Spawn-time env for the daemon: coordinator base URL.
pub const ENV_COORD_URL: &str = "ENGRAM_EGRESS_PROXYD_COORD_URL";
/// Spawn-time env for the daemon: coordinator bearer token (empty =
/// auth off, dev coords).
pub const ENV_COORD_TOKEN: &str = "ENGRAM_EGRESS_PROXYD_COORD_TOKEN";
/// Spawn-time env for the daemon: this host's `HostId`.
pub const ENV_HOST_ID: &str = "ENGRAM_EGRESS_PROXYD_HOST_ID";

/// The BUILD-time env both binaries embed via `option_env!`: a content
/// hash of the daemon's source dependency closure, computed at image
/// build. `None` (local/dev builds) is never adopted — the caller
/// replaces the daemon rather than guess. Never a binary hash: builds
/// are not reproducible, so a binary hash would restart the daemon on
/// ~every deploy and defeat the design (ADR 0121).
pub const BUILD_ENV_SOURCE_FINGERPRINT: &str = "ENGRAM_EGRESS_PROXYD_FINGERPRINT";

/// Host-agent → proxyd.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum ToProxyd {
    /// First frame on every connection.
    Hello {
        proto_version: u32,
    },
    /// Full replace: register every policy in the set, then drop any
    /// registered session NOT in the set. Sent on every connect —
    /// this is ADR 0111's `rebuild_egress_from_policies` retargeted
    /// at the daemon, and it doubles as the stale-entry prune.
    SyncPolicies(Vec<SessionEgressPolicy>),
    /// Register (or replace) one session's policy. Also the capture
    /// VM path — a capture registration is an ordinary policy under a
    /// synthetic session id.
    ApplyPolicy(Box<SessionEgressPolicy>),
    /// Drop one session's registration and its tunnel state.
    RemoveSession(SessionId),
    Health,
    /// Debug/test: who is registered at this guest IP?
    LookupGuest(std::net::Ipv4Addr),
    /// Debug/test: what would the proxy decide for this guest → host
    /// pair? Answers the decision NAME (`"bypass"` / `"reject"` /
    /// `"intercept"` / `"own-app"`), `None` for an unknown guest.
    Decide {
        guest_ip: std::net::Ipv4Addr,
        host: String,
    },
    /// Exit promptly after acking. The graceful half of an upgrade
    /// replace (`AdoptPlan::RestartForUpgrade`).
    Shutdown,
}

/// proxyd → host-agent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum FromProxyd {
    HelloAck(HelloInfo),
    Ok,
    Err(String),
    HealthReport { sessions: usize },
    Guest(Option<GuestSummary>),
    Decision(Option<String>),
}

/// The `LookupGuest` answer: the registration's identity, without the
/// policy body (which carries resolved secrets and stays inside the
/// daemon).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GuestSummary {
    pub session_id: SessionId,
    pub sandbox_id: SandboxId,
}

/// What a live daemon reports about itself in `HelloAck` position.
/// The adopt decision compares this against what the successor
/// host-agent expects — the daemon's own answer, not the manifest
/// file, is the authority (the file can be stale or torn).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HelloInfo {
    pub proto_version: u32,
    /// The build-time source fingerprint, `None` for local builds.
    pub source_fingerprint: Option<String>,
    pub proxy_port: u16,
    pub dns_port: u16,
    pub gateway_port: u16,
    /// SHA-256 (hex) of the CA cert PEM the daemon serves leaves
    /// under. A rotated CA is a config mismatch → replace.
    pub ca_fingerprint: String,
    pub coord_url: String,
}

/// proxyd → host-agent over the dial-back socket: open an ADR 0118
/// guest stream to `port` inside `sandbox`'s guest and pass the
/// connected fd back via SCM_RIGHTS.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DialbackRequest {
    pub sandbox_id: SandboxId,
    pub port: u16,
}

/// Host-agent → proxyd over the dial-back socket. On `Ok` the
/// connected fd follows as an SCM_RIGHTS control message on a 1-byte
/// carrier (see [`send_fd`]/[`recv_fd`]).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum DialbackResponse {
    Ok,
    Err(String),
}

fn oversize(len: usize) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("egress-proto frame of {len} bytes exceeds cap {MAX_MSG_BYTES}"),
    )
}

/// Read one length-prefixed JSON frame (async).
pub async fn read_frame<R, T>(r: &mut R) -> std::io::Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MSG_BYTES {
        return Err(oversize(len));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Write one length-prefixed JSON frame (async).
pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(msg)?;
    if body.len() > MAX_MSG_BYTES {
        return Err(oversize(body.len()));
    }
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await
}

/// Read one length-prefixed JSON frame (sync — the dial-back path
/// runs on std `UnixStream` so the fd can ride SCM_RIGHTS).
pub fn read_frame_sync<R, T>(r: &mut R) -> std::io::Result<T>
where
    R: std::io::Read,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_MSG_BYTES {
        return Err(oversize(len));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    serde_json::from_slice(&buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Write one length-prefixed JSON frame (sync).
pub fn write_frame_sync<W, T>(w: &mut W, msg: &T) -> std::io::Result<()>
where
    W: std::io::Write,
    T: Serialize,
{
    let body = serde_json::to_vec(msg)?;
    if body.len() > MAX_MSG_BYTES {
        return Err(oversize(body.len()));
    }
    w.write_all(&(body.len() as u32).to_be_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Send `fd` over `sock` via SCM_RIGHTS on a 1-byte carrier (the
/// `engram-substrate-proto` mechanism, re-hosted here because that
/// crate is pinned to the fault path and Linux-only; the dial-back
/// also runs on macOS/VZ).
#[cfg(unix)]
pub fn send_fd(
    sock: &std::os::unix::net::UnixStream,
    fd: std::os::fd::BorrowedFd<'_>,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let carrier = [0xEDu8];
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

    #[tokio::test]
    async fn frames_round_trip_async() {
        let msgs = vec![
            ToProxyd::Hello {
                proto_version: PROTO_VERSION,
            },
            ToProxyd::Health,
            ToProxyd::RemoveSession(SessionId::new()),
            ToProxyd::Shutdown,
        ];
        for m in msgs {
            let mut buf = Vec::new();
            write_frame(&mut buf, &m).await.unwrap();
            let back: ToProxyd = read_frame(&mut buf.as_slice()).await.unwrap();
            assert_eq!(back, m);
        }
        let m = FromProxyd::HelloAck(HelloInfo {
            proto_version: PROTO_VERSION,
            source_fingerprint: Some("abc123".into()),
            proxy_port: 8443,
            dns_port: 5353,
            gateway_port: 13338,
            ca_fingerprint: "deadbeef".into(),
            coord_url: "http://coord:8080".into(),
        });
        let mut buf = Vec::new();
        write_frame(&mut buf, &m).await.unwrap();
        let back: FromProxyd = read_frame(&mut buf.as_slice()).await.unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn frames_round_trip_sync() {
        let m = DialbackRequest {
            sandbox_id: SandboxId::new(),
            port: 3000,
        };
        let mut buf = Vec::new();
        write_frame_sync(&mut buf, &m).unwrap();
        let back: DialbackRequest = read_frame_sync(&mut buf.as_slice()).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn oversize_frame_is_refused_on_read() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&((MAX_MSG_BYTES as u32) + 1).to_be_bytes());
        let err = read_frame_sync::<_, DialbackRequest>(&mut buf.as_slice()).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    #[test]
    fn fd_round_trips_over_a_socketpair() {
        use std::io::{Read as _, Seek as _, Write as _};
        use std::os::fd::AsFd;
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(b"relayed bytes").unwrap();
        f.flush().unwrap();
        send_fd(&a, f.as_fd()).unwrap();
        let received = recv_fd(&b).unwrap();
        let mut file = std::fs::File::from(received);
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        let mut out = String::new();
        file.read_to_string(&mut out).unwrap();
        assert_eq!(out, "relayed bytes");
    }
}
