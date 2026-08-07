//! Filtering DNS proxy for guest microVMs.
//!
//! The TCP/443 proxy enforces `manifest.network.allow_hosts` at SNI
//! peek time — but DNS itself was an unconditional ACCEPT to 1.1.1.1
//! at the iptables layer, which is a classic exfil channel: a
//! compromised harness can encode data into subdomains of an
//! attacker-controlled name and queries leave the host even when
//! every TCP connection downstream is blocked. This module closes
//! that channel by REDIRECTing both `udp/53` and `tcp/53` to a
//! filtering DNS proxy that resolves only names already in
//! `allow_hosts` (the same `HostList` the TCP/443 path uses).
//!
//! Wire shape:
//! - **UDP** (the common case): receive a query, parse it via
//!   `hickory-proto`, pull the first QNAME, check it against the
//!   `SessionState` looked up by source-IP. Allowed → forward
//!   verbatim to an upstream resolver and ship the response back
//!   over the same socket. Denied → synthesize an `NXDOMAIN`
//!   response (preserving the query's EDNS OPT if any) and send it
//!   back. Queries from unknown source IPs (no `SessionState`) are
//!   denied — we silently NXDOMAIN them so the guest's resolver
//!   bails fast instead of timing out.
//! - **TCP** (the >512-byte fallback): same logic, with the 2-byte
//!   length prefix DNS-over-TCP adds.
//!
//! Why this matters even though the FORWARD chain default-DROPs
//! everything: without this, the only thing protecting against
//! exfil was the *explicit* `ACCEPT VM→1.1.1.1:53` iptables rules,
//! which had to exist for any DNS to work. Removing those rules
//! and routing all DNS through this proxy means a guest can only
//! resolve names that the manifest's `allow_hosts` already permits
//! a TCP connection to. Resolution and connection are filtered
//! against the same list.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use hickory_proto::op::{Message, MessageType, ResponseCode};
use hickory_proto::serialize::binary::BinDecodable;
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};

use crate::registry::{Registry, SessionState};

/// Default upstream DNS resolver. Cloudflare's public anycast.
pub const DEFAULT_UPSTREAM: &str = "1.1.1.1:53";

/// How long we'll wait on an upstream UDP response before reaping
/// the pending-entry. RFC 1035 doesn't define a timeout; 5s matches
/// what glibc's resolver does before retrying.
const UDP_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(5);

/// How long we'll wait on an upstream TCP exchange. TCP DNS is the
/// fallback for truncated UDP, so it should be at least as
/// patient as a fresh UDP retry.
const TCP_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// Max DNS message size we'll buffer. EDNS bumps the typical 512
/// limit; 4 KiB is what most resolvers advertise.
const MAX_DGRAM: usize = 4096;

/// Spawn the UDP/53 DNS proxy. Reads queries off `socket`, decides
/// allow/deny per `registry`, forwards allowed queries to
/// `upstream_addr`, and ships the response back to the original
/// guest. Returns only on listener error.
pub async fn serve_udp(
    socket: Arc<UdpSocket>,
    registry: Arc<Registry>,
    upstream_addr: SocketAddr,
) -> std::io::Result<()> {
    // One bound upstream socket per process. Responses come back
    // here keyed by the DNS message ID; the demux loop dispatches
    // them to the per-query reply targets. Production recursive
    // resolvers randomize source ports for spoofing resistance; we
    // sit host-side and only talk to a trusted upstream, so a
    // single bound socket is fine.
    let upstream = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
    tracing::info!(
        listen = %socket.local_addr()?,
        upstream = %upstream_addr,
        "engram-egress-proxy DNS/udp serving",
    );
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

    // Demux loop: read upstream responses, look up the original
    // peer by message ID, send the bytes back. One task for the
    // whole proxy; per-query state lives in `pending`.
    let demux_upstream = upstream.clone();
    let demux_pending = pending.clone();
    let demux_socket = socket.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; MAX_DGRAM];
        loop {
            let (n, _) = match demux_upstream.recv_from(&mut buf).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "upstream DNS recv failed; demux halting");
                    return;
                }
            };
            // DNS message ID is the first two bytes of the header.
            if n < 12 {
                continue;
            }
            let id = u16::from_be_bytes([buf[0], buf[1]]);
            let entry = demux_pending.lock().remove(&id);
            if let Some(peer) = entry {
                let _ = demux_socket.send_to(&buf[..n], peer).await;
            }
        }
    });

    let mut buf = vec![0u8; MAX_DGRAM];
    loop {
        let (n, peer) = socket.recv_from(&mut buf).await?;
        let pkt = buf[..n].to_vec();
        let registry = registry.clone();
        let upstream = upstream.clone();
        let pending = pending.clone();
        let socket = socket.clone();
        tokio::spawn(async move {
            handle_udp(
                pkt,
                peer,
                registry,
                socket,
                upstream,
                pending,
                upstream_addr,
            )
            .await;
        });
    }
}

type PendingMap = Arc<Mutex<HashMap<u16, SocketAddr>>>;

#[allow(clippy::too_many_arguments)]
async fn handle_udp(
    pkt: Vec<u8>,
    peer: SocketAddr,
    registry: Arc<Registry>,
    socket: Arc<UdpSocket>,
    upstream: Arc<UdpSocket>,
    pending: PendingMap,
    upstream_addr: SocketAddr,
) {
    let query = match Message::from_bytes(&pkt) {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(peer = %peer, error = %e, "malformed DNS query; dropping");
            return;
        }
    };
    let guest_ip = match peer.ip() {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(_) => return,
    };
    match decide(&registry, guest_ip, &query) {
        Decision::Allow => {
            // Hold a slot for the upstream response before send to
            // avoid racing the demux loop. Reaped by the timeout
            // sweep below if no reply lands within UDP_UPSTREAM_TIMEOUT.
            let id = query.id();
            pending.lock().insert(id, peer);
            if let Err(e) = upstream.send_to(&pkt, upstream_addr).await {
                tracing::debug!(
                    peer = %peer, error = %e,
                    "upstream DNS send failed",
                );
                pending.lock().remove(&id);
                return;
            }
            // Reaper: if upstream never replies, drop the pending
            // entry so it doesn't leak. The guest will time out and
            // retry on its own.
            let pending_for_reap = pending.clone();
            tokio::spawn(async move {
                tokio::time::sleep(UDP_UPSTREAM_TIMEOUT).await;
                pending_for_reap.lock().remove(&id);
            });
        }
        Decision::Deny(reason) => {
            tracing::debug!(
                peer = %peer,
                qname = ?query.queries().first().map(|q| q.name().to_utf8()),
                reason = ?reason,
                "DNS denied; responding NXDOMAIN",
            );
            let resp = build_nxdomain(&query);
            let bytes = match resp.to_vec() {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(peer = %peer, error = %e, "encode NXDOMAIN failed");
                    return;
                }
            };
            let _ = socket.send_to(&bytes, peer).await;
        }
    }
}

/// Spawn the TCP/53 DNS proxy. DNS-over-TCP is the fallback when a
/// UDP response sets TC=1. Without this listener, dropping the
/// iptables `ACCEPT VM→1.1.1.1 tcp/53` rule would mean any
/// >512-byte response just fails — clients can't retry.
pub async fn serve_tcp(
    listener: TcpListener,
    registry: Arc<Registry>,
    upstream_addr: SocketAddr,
) -> std::io::Result<()> {
    tracing::info!(
        listen = %listener.local_addr()?,
        upstream = %upstream_addr,
        "engram-egress-proxy DNS/tcp serving",
    );
    loop {
        let (stream, peer) = listener.accept().await?;
        let registry = registry.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_tcp(stream, peer, registry, upstream_addr).await {
                tracing::debug!(peer = %peer, error = %e, "TCP DNS exchange ended with error");
            }
        });
    }
}

async fn handle_tcp(
    mut stream: TcpStream,
    peer: SocketAddr,
    registry: Arc<Registry>,
    upstream_addr: SocketAddr,
) -> std::io::Result<()> {
    let guest_ip = match peer.ip() {
        IpAddr::V4(v4) => v4,
        IpAddr::V6(_) => return Ok(()),
    };
    loop {
        // RFC 1035 §4.2.2: 2-byte length prefix in network order.
        let mut len_buf = [0u8; 2];
        if stream.read_exact(&mut len_buf).await.is_err() {
            return Ok(()); // Clean close.
        }
        let msg_len = u16::from_be_bytes(len_buf) as usize;
        if msg_len == 0 || msg_len > MAX_DGRAM {
            return Ok(());
        }
        let mut msg_buf = vec![0u8; msg_len];
        stream.read_exact(&mut msg_buf).await?;

        let query = match Message::from_bytes(&msg_buf) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(peer = %peer, error = %e, "malformed TCP DNS query");
                return Ok(());
            }
        };
        match decide(&registry, guest_ip, &query) {
            Decision::Allow => {
                let resp = forward_tcp(&msg_buf, upstream_addr).await?;
                let len = u16::try_from(resp.len()).map_err(std::io::Error::other)?;
                stream.write_all(&len.to_be_bytes()).await?;
                stream.write_all(&resp).await?;
                stream.flush().await?;
            }
            Decision::Deny(_) => {
                let resp = build_nxdomain(&query);
                let bytes = resp
                    .to_vec()
                    .map_err(|e| std::io::Error::other(format!("encode NXDOMAIN: {e}")))?;
                let len = u16::try_from(bytes.len()).map_err(std::io::Error::other)?;
                stream.write_all(&len.to_be_bytes()).await?;
                stream.write_all(&bytes).await?;
                stream.flush().await?;
            }
        }
    }
}

async fn forward_tcp(query_bytes: &[u8], upstream_addr: SocketAddr) -> std::io::Result<Vec<u8>> {
    let mut up = tokio::time::timeout(TCP_UPSTREAM_TIMEOUT, TcpStream::connect(upstream_addr))
        .await
        .map_err(|_| std::io::Error::other("upstream connect timeout"))??;
    let len = u16::try_from(query_bytes.len()).map_err(std::io::Error::other)?;
    up.write_all(&len.to_be_bytes()).await?;
    up.write_all(query_bytes).await?;
    up.flush().await?;
    let mut resp_len_buf = [0u8; 2];
    up.read_exact(&mut resp_len_buf).await?;
    let resp_len = u16::from_be_bytes(resp_len_buf) as usize;
    if resp_len == 0 || resp_len > MAX_DGRAM {
        return Err(std::io::Error::other(format!(
            "upstream DNS response length out of range: {resp_len}",
        )));
    }
    let mut resp = vec![0u8; resp_len];
    up.read_exact(&mut resp).await?;
    Ok(resp)
}

#[derive(Debug)]
enum Decision {
    Allow,
    Deny(DenyReason),
}

#[derive(Debug)]
enum DenyReason {
    UnknownGuest,
    NoQuery,
    NotInAllowList,
}

fn decide(registry: &Registry, guest_ip: Ipv4Addr, query: &Message) -> Decision {
    let Some(state) = registry.lookup(guest_ip) else {
        return Decision::Deny(DenyReason::UnknownGuest);
    };
    let Some(q) = query.queries().first() else {
        return Decision::Deny(DenyReason::NoQuery);
    };
    let qname = q
        .name()
        .to_utf8()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if name_allowed(&state, &qname) {
        Decision::Allow
    } else {
        Decision::Deny(DenyReason::NotInAllowList)
    }
}

/// May the guest resolve this name?
///
/// This must answer the same question [`SessionState::decide`] answers for TCP,
/// or a host the proxy is willing to intercept is one the guest cannot look up.
/// A resolvable-but-refused name is fine (the connection is dropped); a
/// reachable-but-unresolvable one is a broken integration.
///
/// So every source of reachability counts: the network allow-list, and any
/// secret, injection or observe spec that names the host. Injections used to be
/// missing, which only worked because a Google or Datadog host was ALSO in the
/// network allow-list. ADR 0109 stopped adding a Google host there — a failed
/// mint must leave nothing reachable — and that immediately made the inject
/// hosts unresolvable.
///
/// A credential-exchange host is refused outright, matching `decide()`.
fn name_allowed(state: &SessionState, qname: &str) -> bool {
    if crate::google_denylist::denies_host(qname) {
        return false;
    }
    if state.allow_all || state.network_allow.matches(qname) {
        return true;
    }
    state.secrets.iter().any(|s| s.allow.matches(qname))
        || state.injects.iter().any(|i| i.allow.matches(qname))
        || state.observes.iter().any(|o| o.allow.matches(qname))
}

fn build_nxdomain(query: &Message) -> Message {
    let mut resp = Message::new();
    resp.set_id(query.id());
    resp.set_message_type(MessageType::Response);
    resp.set_op_code(query.op_code());
    resp.set_recursion_desired(query.recursion_desired());
    resp.set_recursion_available(true);
    resp.set_response_code(ResponseCode::NXDomain);
    for q in query.queries() {
        resp.add_query(q.clone());
    }
    // Echo EDNS OPT if the query carried one — clients use the
    // version/flags field of OPT for capability negotiation.
    if let Some(edns) = query.extensions() {
        resp.set_edns(edns.clone());
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::HostList;
    use crate::registry::SessionState;
    use engram_core::SessionId;
    use hickory_proto::op::Query;
    use hickory_proto::rr::{Name, RecordType};
    use std::str::FromStr;

    fn make_query(name: &str, rtype: RecordType) -> Message {
        let mut q = Message::new();
        q.set_id(0x4242);
        q.set_message_type(MessageType::Query);
        q.set_recursion_desired(true);
        let qname = Name::from_str(name).unwrap();
        q.add_query(Query::query(qname, rtype));
        q
    }

    fn registry_with(state: SessionState) -> Arc<Registry> {
        let r = Arc::new(Registry::new());
        r.register(state);
        r
    }

    #[test]
    fn allows_exact_match_in_network_allow() {
        let state = SessionState {
            session_id: SessionId::new(),
            guest_ip: Ipv4Addr::new(10, 200, 0, 2),
            allow_all: false,
            network_allow: HostList::from_manifest(&["api.anthropic.com".into()], &[]).unwrap(),
            secrets: Vec::new(),
            injects: Vec::new(),
            observes: Vec::new(),
            guest_services: Vec::new(),
            tunnels: Vec::new(),
        };
        let reg = registry_with(state);
        let q = make_query("api.anthropic.com.", RecordType::A);
        assert!(matches!(
            decide(&reg, Ipv4Addr::new(10, 200, 0, 2), &q),
            Decision::Allow
        ));
    }

    #[test]
    fn resolves_a_host_reachable_only_through_an_injection() {
        // ADR 0109: a Google host is no longer added to the network allow-list
        // — a failed mint must leave nothing reachable. Reachability rides the
        // injection itself, so resolution has to follow it or the guest cannot
        // look the host up at all.
        let inject = crate::registry::InjectEntry {
            header_name: "authorization".into(),
            header_template: "Bearer {}".into(),
            allow: HostList::from_manifest(&["compute.googleapis.com".into()], &[]).unwrap(),
            policy: crate::registry::RequestPolicy::default(),
            mint_source: None,
            cred: crate::registry::RefreshableCred::new("token".into(), None),
        };
        let state = SessionState {
            session_id: SessionId::new(),
            guest_ip: Ipv4Addr::new(10, 200, 0, 2),
            allow_all: false,
            network_allow: HostList::from_manifest(&[], &[]).unwrap(),
            secrets: Vec::new(),
            injects: vec![inject],
            observes: Vec::new(),
            guest_services: Vec::new(),
            tunnels: Vec::new(),
        };
        let reg = registry_with(state);
        let query = make_query("compute.googleapis.com.", RecordType::A);
        assert!(matches!(
            decide(&reg, Ipv4Addr::new(10, 200, 0, 2), &query),
            Decision::Allow
        ));
    }

    #[test]
    fn refuses_to_resolve_a_credential_exchange_host() {
        let state = SessionState {
            session_id: SessionId::new(),
            guest_ip: Ipv4Addr::new(10, 200, 0, 2),
            allow_all: true,
            network_allow: HostList::from_manifest(&[], &[]).unwrap(),
            secrets: Vec::new(),
            injects: Vec::new(),
            observes: Vec::new(),
            guest_services: Vec::new(),
            tunnels: Vec::new(),
        };
        let reg = registry_with(state);
        for name in ["sts.googleapis.com.", "sts.mtls.googleapis.com."] {
            let query = make_query(name, RecordType::A);
            assert!(
                matches!(
                    decide(&reg, Ipv4Addr::new(10, 200, 0, 2), &query),
                    Decision::Deny(DenyReason::NotInAllowList)
                ),
                "{name}",
            );
        }
    }

    #[test]
    fn allows_wildcard_pattern() {
        let state = SessionState {
            session_id: SessionId::new(),
            guest_ip: Ipv4Addr::new(10, 200, 0, 2),
            allow_all: false,
            network_allow: HostList::from_manifest(&[], &["*.anthropic.com".into()]).unwrap(),
            secrets: Vec::new(),
            injects: Vec::new(),
            observes: Vec::new(),
            guest_services: Vec::new(),
            tunnels: Vec::new(),
        };
        let reg = registry_with(state);
        let q = make_query("api.anthropic.com.", RecordType::A);
        assert!(matches!(
            decide(&reg, Ipv4Addr::new(10, 200, 0, 2), &q),
            Decision::Allow
        ));
    }

    #[test]
    fn denies_unknown_guest_ip() {
        let reg = Arc::new(Registry::new());
        let q = make_query("api.anthropic.com.", RecordType::A);
        assert!(matches!(
            decide(&reg, Ipv4Addr::new(10, 200, 0, 99), &q),
            Decision::Deny(DenyReason::UnknownGuest)
        ));
    }

    #[test]
    fn denies_name_not_in_allow_list() {
        let state = SessionState {
            session_id: SessionId::new(),
            guest_ip: Ipv4Addr::new(10, 200, 0, 2),
            allow_all: false,
            network_allow: HostList::from_manifest(&["api.anthropic.com".into()], &[]).unwrap(),
            secrets: Vec::new(),
            injects: Vec::new(),
            observes: Vec::new(),
            guest_services: Vec::new(),
            tunnels: Vec::new(),
        };
        let reg = registry_with(state);
        let q = make_query("evil.example.com.", RecordType::A);
        assert!(matches!(
            decide(&reg, Ipv4Addr::new(10, 200, 0, 2), &q),
            Decision::Deny(DenyReason::NotInAllowList)
        ));
    }

    #[test]
    fn nxdomain_response_preserves_id_and_question() {
        let q = make_query("evil.example.com.", RecordType::AAAA);
        let resp = build_nxdomain(&q);
        assert_eq!(resp.id(), q.id());
        assert_eq!(resp.message_type(), MessageType::Response);
        assert_eq!(resp.response_code(), ResponseCode::NXDomain);
        assert!(resp.recursion_desired());
        assert!(resp.recursion_available());
        assert_eq!(resp.queries().len(), 1);
        assert_eq!(resp.queries()[0].name(), q.queries()[0].name());
        assert_eq!(resp.queries()[0].query_type(), RecordType::AAAA);
    }

    #[test]
    fn allows_via_secret_allow_list() {
        // A name covered only by a per-secret allow list (not in
        // network_allow) should still resolve, mirroring the TCP
        // path's `Intercept` decision.
        let state = SessionState {
            session_id: SessionId::new(),
            guest_ip: Ipv4Addr::new(10, 200, 0, 2),
            allow_all: false,
            network_allow: HostList::empty(),
            secrets: vec![crate::registry::SecretEntry {
                placeholder: "engram_ph_test".into(),
                real_value: "sk-test".into(),
                allow: HostList::from_manifest(&["api.anthropic.com".into()], &[]).unwrap(),
            }],
            injects: Vec::new(),
            observes: Vec::new(),
            guest_services: Vec::new(),
            tunnels: Vec::new(),
        };
        let reg = registry_with(state);
        let q = make_query("api.anthropic.com.", RecordType::A);
        assert!(matches!(
            decide(&reg, Ipv4Addr::new(10, 200, 0, 2), &q),
            Decision::Allow
        ));
    }

    #[tokio::test]
    async fn udp_end_to_end_allowed_query_round_trips() {
        // Wire a real socket-pair: a fake upstream UDP echoes a
        // fixed A response; the proxy forwards an allowed query;
        // the client receives the response.
        let upstream = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();
        // Upstream behaviour: turn the inbound query into a NOERROR
        // answer with one A record pointing at 192.0.2.1 (TEST-NET-1).
        let upstream_task = tokio::spawn(async move {
            let mut buf = vec![0u8; MAX_DGRAM];
            let (n, peer) = upstream.recv_from(&mut buf).await.unwrap();
            let query = Message::from_bytes(&buf[..n]).unwrap();
            let mut resp = Message::new();
            resp.set_id(query.id());
            resp.set_message_type(MessageType::Response);
            resp.set_response_code(ResponseCode::NoError);
            for q in query.queries() {
                resp.add_query(q.clone());
            }
            let bytes = resp.to_vec().unwrap();
            upstream.send_to(&bytes, peer).await.unwrap();
        });

        // Bind the proxy on an ephemeral port.
        let proxy_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let proxy_addr = proxy_sock.local_addr().unwrap();
        let registry = Arc::new(Registry::new());
        let client_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client_sock.local_addr().unwrap();
        let IpAddr::V4(client_ip) = client_addr.ip() else {
            panic!("client must be v4");
        };
        registry.register(SessionState {
            session_id: SessionId::new(),
            guest_ip: client_ip,
            allow_all: false,
            network_allow: HostList::from_manifest(&["allowed.example.com".into()], &[]).unwrap(),
            secrets: Vec::new(),
            injects: Vec::new(),
            observes: Vec::new(),
            guest_services: Vec::new(),
            tunnels: Vec::new(),
        });
        let proxy_task = tokio::spawn(serve_udp(proxy_sock.clone(), registry, upstream_addr));

        let query = make_query("allowed.example.com.", RecordType::A);
        let q_bytes = query.to_vec().unwrap();
        client_sock.send_to(&q_bytes, proxy_addr).await.unwrap();

        let mut buf = vec![0u8; MAX_DGRAM];
        let (n, _) = tokio::time::timeout(Duration::from_secs(3), client_sock.recv_from(&mut buf))
            .await
            .expect("client receives response")
            .unwrap();
        let resp = Message::from_bytes(&buf[..n]).unwrap();
        assert_eq!(resp.id(), query.id());
        assert_eq!(resp.response_code(), ResponseCode::NoError);
        upstream_task.await.unwrap();
        proxy_task.abort();
    }

    #[tokio::test]
    async fn udp_end_to_end_denied_query_returns_nxdomain() {
        let proxy_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let proxy_addr = proxy_sock.local_addr().unwrap();
        let registry = Arc::new(Registry::new());
        let client_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client_sock.local_addr().unwrap();
        let IpAddr::V4(client_ip) = client_addr.ip() else {
            panic!("client must be v4");
        };
        registry.register(SessionState {
            session_id: SessionId::new(),
            guest_ip: client_ip,
            allow_all: false,
            network_allow: HostList::from_manifest(&["allowed.example.com".into()], &[]).unwrap(),
            secrets: Vec::new(),
            injects: Vec::new(),
            observes: Vec::new(),
            guest_services: Vec::new(),
            tunnels: Vec::new(),
        });
        // Point upstream at an obviously-dead address so the test
        // can't accidentally succeed by hitting a real resolver.
        let upstream_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let proxy_task = tokio::spawn(serve_udp(proxy_sock.clone(), registry, upstream_addr));

        let query = make_query("evil.example.com.", RecordType::A);
        let q_bytes = query.to_vec().unwrap();
        client_sock.send_to(&q_bytes, proxy_addr).await.unwrap();

        let mut buf = vec![0u8; MAX_DGRAM];
        let (n, _) = tokio::time::timeout(Duration::from_secs(3), client_sock.recv_from(&mut buf))
            .await
            .expect("client receives response")
            .unwrap();
        let resp = Message::from_bytes(&buf[..n]).unwrap();
        assert_eq!(resp.id(), query.id());
        assert_eq!(resp.response_code(), ResponseCode::NXDomain);
        proxy_task.abort();
    }
}
