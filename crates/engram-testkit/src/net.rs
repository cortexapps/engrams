//! Port selection for tests that start a listener on a FIXED port.
//!
//! The egress daemon refuses port 0 (it must bind the literal ports that
//! the guest redirects point at), so a test picks free ports and passes
//! them in. The daemon binds every port on the wildcard address, and it
//! binds the DNS port for BOTH TCP and UDP.
//!
//! A probe that binds only TCP on `127.0.0.1` cannot see a UDP socket
//! that another process holds on the same port number. TCP and UDP
//! ephemeral ports come from one range, so the probe can return a port
//! that a UDP socket of a parallel test process already holds. The
//! daemon's UDP bind on `0.0.0.0:<port>` then fails `EADDRINUSE` on
//! every attempt, and the daemon exits before it is ready.

use std::net::{Ipv4Addr, TcpListener, UdpSocket};

/// A port that is free, at the moment of the call, for a TCP listener
/// AND a UDP socket on the wildcard address — the binds the egress
/// daemon makes. The probe sockets close before return, so the caller
/// must bind the port at once.
pub fn free_tcp_udp_port() -> u16 {
    // Each pass takes a new ephemeral port from the kernel. A port whose
    // UDP side is in use is skipped; the bound is a guard against a
    // host with no free pair at all, not a wait.
    for _ in 0..64 {
        let tcp = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("ephemeral TCP bind");
        let port = tcp.local_addr().expect("local addr").port();
        if UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).is_ok() {
            return port;
        }
    }
    panic!("no port free for both TCP and UDP in 64 ephemeral picks");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_port_binds_for_tcp_and_udp_on_the_wildcard_address() {
        let port = free_tcp_udp_port();
        let _tcp = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).expect("TCP bind");
        let _udp = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).expect("UDP bind");
    }

    #[test]
    fn a_port_whose_udp_side_is_held_is_never_returned() {
        // Hold the UDP side of many ephemeral ports, then check that no
        // returned port is one of them.
        let held: Vec<UdpSocket> = (0..256)
            .map(|_| UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("UDP bind"))
            .collect();
        let held_ports: std::collections::HashSet<u16> = held
            .iter()
            .map(|s| s.local_addr().expect("local addr").port())
            .collect();
        for _ in 0..256 {
            assert!(!held_ports.contains(&free_tcp_udp_port()));
        }
    }
}
