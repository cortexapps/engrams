//! ADR 0064: ProxyPort types — the raw-byte sibling of [`super::shell`].
//!
//! Where [`super::shell::ShellTunnel`] carries WebSocket frames to the
//! in-guest ttyd (pinned to :7681), a [`PortTunnel`] carries **opaque
//! TCP bytes** to an **arbitrary** guest port, so a dev server the agent
//! started (`localhost:3000`) is reachable from the web at a vanity
//! subdomain. There is no frame taxonomy: the bytes are whatever the
//! inner protocol speaks (HTTP/1.1, h2c, a WebSocket upgrade, gRPC).
//! Channel closure on either side is the EOF/teardown signal.
//!
//! Kept in `engram-core` (like `ShellTunnel`) so the `HostClient` trait
//! surface doesn't leak the gRPC proto type into every implementer.

use bytes::Bytes;
use tokio::sync::mpsc;

/// Per-stream capacity for both `outbound` (coord → host) and `inbound`
/// (host → coord). 64 chunks of up to ~64 KiB keeps a few MiB buffered
/// when one side momentarily blocks — enough to smooth bursty HTTP
/// responses without unbounded memory growth (backpressure kicks in at
/// the channel boundary).
pub const PORT_TUNNEL_CHANNEL_CAPACITY: usize = 64;

/// Handle on a bidirectional raw-byte proxy to one guest TCP port.
/// Caller (coord) writes guest-bound bytes into [`Self::outbound`] and
/// drains guest-produced bytes from [`Self::inbound`]. Either channel
/// closing — by either side — tears down the tunnel (the host-side pump
/// half-closes the socket and exits).
pub struct PortTunnel {
    pub outbound: mpsc::Sender<Bytes>,
    pub inbound: mpsc::Receiver<Bytes>,
}

impl PortTunnel {
    /// Build a `(PortTunnel, PortTunnelEnds)` pair where the returned
    /// [`PortTunnelEnds`] is the opposite end for the proxy
    /// implementation to plug into the guest socket / gRPC stream. Used
    /// by both the in-process and gRPC `HostClient` impls so the channel
    /// orientation is consistent at every layer (mirrors
    /// [`super::shell::ShellTunnel::pair`]).
    pub fn pair() -> (Self, PortTunnelEnds) {
        let (out_tx, out_rx) = mpsc::channel(PORT_TUNNEL_CHANNEL_CAPACITY);
        let (in_tx, in_rx) = mpsc::channel(PORT_TUNNEL_CHANNEL_CAPACITY);
        (
            PortTunnel {
                outbound: out_tx,
                inbound: in_rx,
            },
            PortTunnelEnds {
                outbound_rx: out_rx,
                inbound_tx: in_tx,
            },
        )
    }
}

/// The implementation-side counterpart to [`PortTunnel`]: drains
/// outbound bytes the caller wrote and pushes inbound bytes to the
/// caller.
pub struct PortTunnelEnds {
    pub outbound_rx: mpsc::Receiver<Bytes>,
    pub inbound_tx: mpsc::Sender<Bytes>,
}
