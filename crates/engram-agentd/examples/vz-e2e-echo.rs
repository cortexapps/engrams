//! Guest-side TCP echo + sink servers for the VZ live e2e's
//! head-of-line-blocking test (`e2e_vz_port_relay_reaches_loopback_without_hol`).
//!
//! Replaces the `socat` the old test assumed was baked into the demo
//! image: there is no maintained pinned static arm64 socat to download,
//! but we already cross-build this crate for the guest
//! (aarch64-unknown-linux-musl) when staging the agentd bundle, so a
//! ~50-line std-only example rides the same toolchain for free.
//! `make-test-rootfs.sh` installs it at `/usr/bin/vz-e2e-echo`.
//!
//! Usage: `vz-e2e-echo <echo_port> <sink_port>` — binds both on
//! 127.0.0.1 (the whole point: prove the vsock relay reaches guest
//! loopback), prints `ready` on stdout once both listeners are bound,
//! then serves forever:
//!   - echo: every connection gets its own thread copying bytes back,
//!   - sink: accepts and holds connections open, reading and
//!     discarding, never writing (stands in for a stalled HMR
//!     WebSocket / noVNC stream).
//!
//! std-only — no tokio in the guest test helper.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

fn main() {
    let mut args = std::env::args().skip(1);
    let usage = "usage: vz-e2e-echo <echo_port> <sink_port>";
    let echo_port: u16 = args.next().expect(usage).parse().expect(usage);
    let sink_port: u16 = args.next().expect(usage).parse().expect(usage);

    let echo = TcpListener::bind(("127.0.0.1", echo_port)).expect("bind echo");
    let sink = TcpListener::bind(("127.0.0.1", sink_port)).expect("bind sink");
    println!("ready");

    std::thread::spawn(move || {
        for conn in sink.incoming().flatten() {
            std::thread::spawn(move || hold_open(conn));
        }
    });
    for conn in echo.incoming().flatten() {
        std::thread::spawn(move || echo_conn(conn));
    }
}

fn echo_conn(mut conn: TcpStream) {
    let mut buf = [0u8; 4096];
    while let Ok(n) = conn.read(&mut buf) {
        if n == 0 || conn.write_all(&buf[..n]).is_err() {
            break;
        }
    }
}

fn hold_open(mut conn: TcpStream) {
    // Read and discard so the peer's writes always make progress;
    // never reply. The connection stays open until the peer closes.
    let mut buf = [0u8; 4096];
    while let Ok(n) = conn.read(&mut buf) {
        if n == 0 {
            break;
        }
    }
}
