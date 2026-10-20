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

/// `N` distinct ports that are free, at the moment of the call, for a
/// TCP listener AND a UDP socket on the wildcard address — the binds the
/// egress daemon makes. Every probe socket stays open until all `N` ports
/// are picked, so the kernel cannot hand out one port twice. The probes
/// close before return, so the caller must bind the ports at once.
pub fn free_tcp_udp_ports<const N: usize>() -> [u16; N] {
    let mut held = Vec::with_capacity(N);
    // Each pass takes a new ephemeral port from the kernel. A port whose
    // UDP side is in use is skipped; the bound is a guard against a
    // host with no free pair at all, not a wait.
    for _ in 0..64 * N.max(1) {
        if held.len() == N {
            break;
        }
        let tcp = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("ephemeral TCP bind");
        let port = tcp.local_addr().expect("local addr").port();
        if let Ok(udp) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)) {
            held.push((port, tcp, udp));
        }
    }
    assert_eq!(held.len(), N, "no {N} ports free for both TCP and UDP");
    std::array::from_fn(|i| held[i].0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ports_are_distinct_and_bind_for_tcp_and_udp_on_the_wildcard_address() {
        let ports: [u16; 3] = free_tcp_udp_ports();
        assert_eq!(
            ports.iter().collect::<std::collections::HashSet<_>>().len(),
            3,
            "ports must be distinct: {ports:?}"
        );
        for port in ports {
            let _tcp = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).expect("TCP bind");
            let _udp = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).expect("UDP bind");
        }
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
            let [port] = free_tcp_udp_ports();
            assert!(!held_ports.contains(&port));
        }
    }
}
