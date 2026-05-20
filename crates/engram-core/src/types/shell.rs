//! ADR 0014 issue #6: ProxyShell types.
//!
//! The dashboard's SHELL tab speaks WebSocket directly to the
//! coordinator over `/sessions/:id/shell`. Pre-M1.16 the coordinator
//! forwarded straight to `ws://<guest_ip>:7681/ws` on the FC host VM,
//! but coord pods in GKE have no route to the per-VM `guest_ip`
//! (10.200.0.x lives behind a TAP on the FC host VM, or behind a
//! per-VM netns for warm-restored sandboxes). So the proxy now
//! tunnels WS frames through the existing coord ↔ host-agent gRPC
//! channel: host-agent dials ttyd in the right network namespace and
//! shuttles WS frames across the tunnel.
//!
//! `ShellFrame` is the in-process representation of one WebSocket
//! message that flows through the tunnel — kept in `engram-core` so
//! the `HostClient` trait surface doesn't leak the gRPC proto type
//! into every implementer.

use bytes::Bytes;
use tokio::sync::mpsc;

/// Per-stream capacity for both `outbound` (coord → host) and
/// `inbound` (host → coord). 64 frames keeps ~kilobytes of ttyd
/// output buffered when one side momentarily blocks (reasonable; ttyd
/// chunks small writes).
pub const SHELL_TUNNEL_CHANNEL_CAPACITY: usize = 64;

/// ADR 0014 issue #6: handle on a bidirectional shell proxy. Caller
/// (coord) writes browser-bound WS frames into [`Self::outbound`] and
/// drains ttyd-bound WS frames from [`Self::inbound`]. Both channels
/// closing — by either side — tears down the tunnel.
pub struct ShellTunnel {
    pub outbound: mpsc::Sender<ShellFrame>,
    pub inbound: mpsc::Receiver<ShellFrame>,
}

impl ShellTunnel {
    /// Build a pair of `(ShellTunnel, ShellTunnelEnds)` where the
    /// returned `ShellTunnelEnds` is the *opposite* end for the proxy
    /// implementation to plug into ttyd / gRPC. Used by both the
    /// in-process and gRPC `HostClient` impls so the channel
    /// orientation is consistent at every layer.
    pub fn pair() -> (Self, ShellTunnelEnds) {
        let (out_tx, out_rx) = mpsc::channel(SHELL_TUNNEL_CHANNEL_CAPACITY);
        let (in_tx, in_rx) = mpsc::channel(SHELL_TUNNEL_CHANNEL_CAPACITY);
        (
            ShellTunnel {
                outbound: out_tx,
                inbound: in_rx,
            },
            ShellTunnelEnds {
                outbound_rx: out_rx,
                inbound_tx: in_tx,
            },
        )
    }
}

/// The implementation-side counterpart to [`ShellTunnel`]: drains
/// outbound frames the caller wrote and pushes inbound frames to
/// the caller.
pub struct ShellTunnelEnds {
    pub outbound_rx: mpsc::Receiver<ShellFrame>,
    pub inbound_tx: mpsc::Sender<ShellFrame>,
}

/// One WebSocket frame, used for both directions of the proxy
/// tunnel. Mirrors the variants of `tokio_tungstenite::Message` and
/// `axum::extract::ws::Message` — translation at the WS boundary on
/// either end is 1:1.
#[derive(Clone, Debug)]
pub enum ShellFrame {
    Text(String),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    Close(Option<ShellClose>),
}

#[derive(Clone, Debug)]
pub struct ShellClose {
    pub code: u16,
    pub reason: String,
}
