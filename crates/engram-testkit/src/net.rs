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
    // Each candidate is a new ephemeral port from the kernel. The bound is
    // a guard against a host with no free pair at all, not a wait.
    let candidates = std::iter::repeat_with(|| {
        TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("ephemeral TCP bind")
    })
    .take(64 * N.max(1));
    pick(candidates).map(|(port, _, _)| port)
}

/// Takes `N` candidates whose port is also free for UDP on the wildcard
/// address, and returns them with both probe sockets still open. A
/// candidate whose UDP side is in use is skipped.
fn pick<const N: usize>(
    candidates: impl Iterator<Item = TcpListener>,
) -> [(u16, TcpListener, UdpSocket); N] {
    let mut held = Vec::with_capacity(N);
    for tcp in candidates {
        if held.len() == N {
            break;
        }
        let port = tcp.local_addr().expect("local addr").port();
        if let Ok(udp) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)) {
            held.push((port, tcp, udp));
        }
    }
    let found = held.len();
    held.try_into()
        .unwrap_or_else(|_| panic!("{found} of {N} ports free for both TCP and UDP"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests check the sockets that `pick` holds, never a port after
    // its probes close: another test process can take a closed port at any
    // time, so a re-bind after return is not a property of this code.

    #[test]
    fn picked_ports_are_distinct_and_held_for_tcp_and_udp_on_the_wildcard_address() {
        let candidates = std::iter::repeat_with(|| {
            TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("ephemeral TCP bind")
        })
        .take(192);
        let picked: [_; 3] = pick(candidates);
        let ports: std::collections::HashSet<u16> = picked.iter().map(|p| p.0).collect();
        assert_eq!(ports.len(), 3, "ports must be distinct: {ports:?}");
        for (port, tcp, udp) in &picked {
            let tcp = tcp.local_addr().expect("tcp addr");
            let udp = udp.local_addr().expect("udp addr");
            assert_eq!(
                (tcp.ip(), tcp.port()),
                (Ipv4Addr::UNSPECIFIED.into(), *port)
            );
            assert_eq!(
                (udp.ip(), udp.port()),
                (Ipv4Addr::UNSPECIFIED.into(), *port)
            );
        }
    }

    #[test]
    fn a_candidate_whose_udp_side_is_held_is_skipped() {
        // A port whose UDP side another socket holds on 127.0.0.1, with its
        // TCP side free: a TCP-only probe would return it.
        let (busy_udp, busy_tcp) = loop {
            let udp = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("UDP bind");
            let port = udp.local_addr().expect("local addr").port();
            if let Ok(tcp) = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)) {
                break (udp, tcp);
            }
        };
        let busy = busy_udp.local_addr().expect("local addr").port();
        let fallback = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("TCP bind");
        let [(port, _, _)] = pick([busy_tcp, fallback].into_iter());
        assert_ne!(port, busy, "a port with its UDP side in use was returned");
    }
}
