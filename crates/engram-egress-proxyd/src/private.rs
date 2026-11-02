//! Allocation-owned private ingress. No guest listener or general proxy API.
use engram_core::{HostId, SessionId};
use engram_egress_proto::private::{self as wire, Allocation};
use engram_egress_proxy::{Proxy, Registry};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::{
    net::UnixStream,
    sync::{watch, Mutex},
};

type Slot = Arc<Mutex<SlotState>>;
#[derive(Default)]
struct SlotState {
    live: Option<Live>,
    last: Option<(String, u64)>,
    incarnations: HashSet<String>,
}
struct Live {
    cancel: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}
#[derive(Default)]
pub(crate) struct Ingress {
    stopped: std::sync::atomic::AtomicBool,
    slots: Mutex<HashMap<SessionId, Slot>>,
    used: Mutex<HashSet<(String, String, u64)>>,
}
impl Ingress {
    pub(crate) async fn shutdown(&self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        let slots: Vec<_> = self.slots.lock().await.values().cloned().collect();
        for slot in slots {
            if let Some(mut live) = slot.lock().await.live.take() {
                live.cancel.send_replace(true);
                while !*live.done.borrow_and_update() {
                    if live.done.changed().await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    pub(crate) async fn serve(
        &self,
        mut stream: UnixStream,
        allocation: Allocation,
        registry: Arc<Registry>,
        proxy: Arc<Proxy>,
        host: HostId,
        incarnation: &str,
    ) -> std::io::Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use wire::{Operation, Reply, Request, VERSION};
        allocation.validate()?;
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return Err(wire::invalid());
        }
        if allocation.host_id != host || allocation.proxy_incarnation != incarnation {
            return Err(wire::invalid());
        }
        {
            // Bounded replay tombstones live for this proxy incarnation. Once
            // exhausted, fail closed until an operator replaces the daemon.
            let mut used = self.used.lock().await;
            if used.len() >= 4096
                || !used.insert((
                    allocation.host_incarnation.clone(),
                    allocation.boot_attempt.clone(),
                    allocation.attachment_generation,
                ))
            {
                return Err(wire::invalid());
            }
        }
        let slot = self
            .slots
            .lock()
            .await
            .entry(allocation.session_id)
            .or_insert_with(|| Arc::new(Mutex::new(SlotState::default())))
            .clone();
        let mut slot = slot.lock().await;
        if let Some((incarnation, generation)) = &slot.last {
            if (incarnation == &allocation.host_incarnation
                && allocation.attachment_generation <= *generation)
                || (incarnation != &allocation.host_incarnation
                    && slot.incarnations.contains(&allocation.host_incarnation))
            {
                return Err(wire::invalid());
            }
        }
        if let Some(mut old) = slot.live.take() {
            old.cancel.send_replace(true);
            while !*old.done.borrow_and_update() {
                if old.done.changed().await.is_err() {
                    break;
                }
            }
        }
        // The session lease precedes all policy reads. Policy replacement
        // cancels this allocation and waits for every task below to be joined.
        let (session, mut lease) = registry
            .enroll_session(allocation.session_id)
            .ok_or_else(wire::invalid)?;
        let policy = session.managed_deny.as_ref().ok_or_else(wire::invalid)?;
        if session.sandbox_id != allocation.sandbox_id
            || session.guest_ip != allocation.guest_ip
            || policy.revision != allocation.policy_revision
            || policy.digest != allocation.policy_digest
        {
            return Err(wire::invalid());
        }
        let (cancel, mut cancelled) = watch::channel(false);
        let (done, done_rx) = watch::channel(false);
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return Err(wire::invalid());
        }
        slot.last = Some((
            allocation.host_incarnation.clone(),
            allocation.attachment_generation,
        ));
        slot.incarnations
            .insert(allocation.host_incarnation.clone());
        slot.live = Some(Live {
            cancel,
            done: done_rx,
        });
        drop(slot);
        let mut tasks = tokio::task::JoinSet::new();
        let dns_budget = Arc::new(tokio::sync::Semaphore::new(wire::MAX_DNS_FLOWS));
        let operation = async {
            wire::write(
                &mut stream,
                &Reply {
                    version: VERSION,
                    sequence: 0,
                    epoch: 0,
                    state: "closed".into(),
                },
            )
            .await?;
            let mut sequence = 0u64;
            let mut bound_epoch = None;
            loop {
                // A frame read must not be canceled when an unrelated flow
                // completes: it may already have consumed part of the header.
                let request: Request = match wire::read(&mut stream).await {
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                    other => other?,
                };
                while tasks.try_join_next().is_some() {}
                if request.version != VERSION
                    || sequence.checked_add(1) != Some(request.sequence)
                    || request.epoch == 0
                    || request.epoch > 9_007_199_254_740_991
                {
                    return Err(wire::invalid());
                }
                sequence = request.sequence;
                let state = match request.operation {
                    Operation::Bind if bound_epoch.is_none() => {
                        bound_epoch = Some(request.epoch);
                        "ready"
                    }
                    Operation::Close if bound_epoch == Some(request.epoch) => {
                        return Ok(Some(Reply {
                            version: VERSION,
                            sequence,
                            epoch: request.epoch,
                            state: "closed".into(),
                        }))
                    }
                    Operation::Tcp { destination, port } | Operation::Dns { destination, port } => {
                        if bound_epoch != Some(request.epoch)
                            || port == 0
                            || destination.is_unspecified()
                            || destination.is_multicast()
                            || tasks.len() >= wire::MAX_FLOWS
                        {
                            return Err(wire::invalid());
                        }
                        let dns = matches!(request.operation, Operation::Dns { .. });
                        if dns && (destination != allocation.gateway_ip || port != 53) {
                            return Err(wire::invalid());
                        }
                        // Gateway DNS must use the controlled decoder, including TCP.
                        if !dns && destination == allocation.gateway_ip && port == 53 {
                            return Err(wire::invalid());
                        }
                        let permit = if dns {
                            Some(
                                dns_budget
                                    .clone()
                                    .try_acquire_owned()
                                    .map_err(|_| wire::invalid())?,
                            )
                        } else {
                            None
                        };
                        let mut fd = tokio::time::timeout(
                            std::time::Duration::from_secs(wire::HANDOFF_SECONDS),
                            wire::receive_stream(&stream),
                        )
                        .await
                        .map_err(|_| wire::invalid())??;
                        let proxy = proxy.clone();
                        let session = session.clone();
                        tasks.spawn(async move {
                            let _permit = permit;
                            if dns {
                                let _ = tokio::time::timeout(
                                    std::time::Duration::from_secs(10),
                                    async {
                                        let len = fd.read_u16().await? as usize;
                                        if len == 0 || len > wire::MAX_DNS {
                                            return Err(wire::invalid());
                                        }
                                        let mut query = vec![0; len];
                                        fd.read_exact(&mut query).await?;
                                        let response =
                                            proxy.resolve_private(&query, &session).await?;
                                        if response.len() > wire::MAX_DNS {
                                            return Err(wire::invalid());
                                        }
                                        fd.write_u16(response.len() as u16).await?;
                                        fd.write_all(&response).await
                                    },
                                )
                                .await;
                            } else {
                                let _ = proxy
                                    .dispatch_private(
                                        fd,
                                        session,
                                        std::net::SocketAddrV4::new(destination, port),
                                    )
                                    .await;
                            }
                        });
                        "accepted"
                    }
                    _ => return Err(wire::invalid()),
                };
                wire::write(
                    &mut stream,
                    &Reply {
                        version: VERSION,
                        sequence,
                        epoch: request.epoch,
                        state: state.into(),
                    },
                )
                .await?;
            }
        };
        let result = tokio::select! { biased;
            _ = lease.cancelled() => Ok(None),
            _ = cancelled.changed() => Ok(None),
            result = operation => result,
        };
        // No task outlives the stable session lease. This includes TLS peeks,
        // HTTP/2 pumps, DNS upstream sockets, and response writes.
        tasks.shutdown().await;
        let closed_reply = if let Ok(Some(reply)) = &result {
            wire::write(&mut stream, reply).await
        } else {
            Ok(())
        };
        let shutdown = stream.shutdown().await;
        // EOF on this reliable stream is the helper's cleanup barrier. The
        // helper closes it only after packet admission and all work stop.
        // A missing barrier must not turn a policy apply into a success ACK.
        let barrier = tokio::time::timeout(
            std::time::Duration::from_secs(wire::HANDOFF_SECONDS),
            wire::drain_to_eof(&stream),
        )
        .await;
        let barrier_failed =
            !matches!(barrier, Ok(Ok(()))) || shutdown.is_err() || closed_reply.is_err();
        if barrier_failed {
            lease.poison();
        }
        drop(stream);
        drop(lease);
        done.send_replace(true);
        if barrier_failed {
            return Err(wire::invalid());
        }
        result.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::{types::managed_deny::ManagedDenyPolicy, SandboxId};
    use engram_egress_proxy::{Ca, CertMint, HostList, ProxyConfig, SessionState};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wire::{Operation, Reply, Request, VERSION};
    struct Fixture {
        ingress: Arc<Ingress>,
        registry: Arc<Registry>,
        proxy: Arc<Proxy>,
        allocation: Allocation,
        ca: Arc<Ca>,
    }
    impl Fixture {
        async fn new() -> Self {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let registry = Arc::new(Registry::new());
            let policy = ManagedDenyPolicy::new(1, ["mcp.example".into()]).unwrap();
            let allocation = Allocation {
                version: VERSION,
                host_id: HostId::new(),
                host_incarnation: "host-1".into(),
                proxy_incarnation: "proxy-1".into(),
                session_id: SessionId::new(),
                sandbox_id: SandboxId::new(),
                boot_attempt: "boot-1".into(),
                attachment_generation: 1,
                guest_ip: "192.0.2.2".parse().unwrap(),
                gateway_ip: "192.0.2.1".parse().unwrap(),
                policy_revision: 1,
                policy_digest: policy.digest.clone(),
            };
            registry
                .apply(SessionState {
                    managed_deny: Some(policy),
                    session_id: allocation.session_id,
                    sandbox_id: allocation.sandbox_id,
                    guest_ip: allocation.guest_ip,
                    network_allow: HostList::from_manifest(&[], &[]).unwrap(),
                    allow_all: true,
                    secrets: vec![],
                    injects: vec![],
                    observes: vec![],
                    guest_services: vec![],
                    tunnels: vec![],
                    apps: vec![],
                })
                .await
                .unwrap();
            let tmp = tempfile::tempdir().unwrap();
            let ca = Arc::new(Ca::load_or_generate(tmp.path()).unwrap());
            let proxy = Arc::new(Proxy::new(ProxyConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                registry.clone(),
                Arc::new(CertMint::new(ca.clone())),
            )));
            Self {
                ingress: Arc::new(Ingress::default()),
                registry,
                proxy,
                allocation,
                ca,
            }
        }
        fn start(
            &self,
            a: Allocation,
        ) -> (UnixStream, tokio::task::JoinHandle<std::io::Result<()>>) {
            let (client, server) = UnixStream::pair().unwrap();
            let ingress = self.ingress.clone();
            let reg = self.registry.clone();
            let proxy = self.proxy.clone();
            let host = self.allocation.host_id;
            (
                client,
                tokio::spawn(
                    async move { ingress.serve(server, a, reg, proxy, host, "proxy-1").await },
                ),
            )
        }
    }
    async fn reply(c: &mut UnixStream) -> Reply {
        tokio::time::timeout(std::time::Duration::from_secs(2), wire::read(c))
            .await
            .unwrap()
            .unwrap()
    }
    async fn request(c: &mut UnixStream, seq: u64, op: Operation) {
        wire::write(
            c,
            &Request {
                version: VERSION,
                sequence: seq,
                epoch: 7,
                operation: op,
            },
        )
        .await
        .unwrap();
    }
    async fn handoff(c: &UnixStream) -> UnixStream {
        use std::os::fd::AsFd;
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let control = c.as_fd().try_clone_to_owned().unwrap();
        engram_egress_proto::send_fd(&std::os::unix::net::UnixStream::from(control), b.as_fd())
            .unwrap();
        a.set_nonblocking(true).unwrap();
        UnixStream::from_std(a).unwrap()
    }
    #[tokio::test]
    async fn private_replacement_drains_pending_tls_before_ack_and_rejects_replay() {
        let f = Fixture::new().await;
        let (mut c, task) = f.start(f.allocation.clone());
        assert_eq!(reply(&mut c).await.state, "closed");
        request(&mut c, 1, Operation::Bind).await;
        assert_eq!(reply(&mut c).await.state, "ready");
        request(
            &mut c,
            2,
            Operation::Tcp {
                destination: "198.51.100.7".parse().unwrap(),
                port: 8443,
            },
        )
        .await;
        let mut flow = handoff(&c).await;
        assert_eq!(reply(&mut c).await.state, "accepted");
        flow.write_all(&[22, 3]).await.unwrap();
        let mut next = f.allocation.clone();
        next.attachment_generation = 2;
        let closed = tokio::spawn(async move {
            assert!(wire::read::<_, Reply>(&mut c).await.is_err());
            drop(c);
        });
        let (mut new, next_task) = f.start(next);
        assert_eq!(reply(&mut new).await.state, "closed");
        let mut b = [0];
        assert_eq!(flow.read(&mut b).await.unwrap(), 0);
        closed.await.unwrap();
        assert!(task.await.unwrap().is_ok());
        let (mut replay, old) = f.start(f.allocation.clone());
        assert!(wire::read::<_, Reply>(&mut replay).await.is_err());
        assert!(old.await.unwrap().is_err());
        drop(new);
        next_task.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn completed_flow_does_not_cancel_a_partial_control_header() {
        let f = Fixture::new().await;
        let (mut c, task) = f.start(f.allocation.clone());
        reply(&mut c).await;
        request(&mut c, 1, Operation::Bind).await;
        reply(&mut c).await;
        request(
            &mut c,
            2,
            Operation::Tcp {
                destination: "198.51.100.7".parse().unwrap(),
                port: 80,
            },
        )
        .await;
        let mut flow = handoff(&c).await;
        reply(&mut c).await;
        let body = serde_json::to_vec(&Request {
            version: VERSION,
            sequence: 3,
            epoch: 7,
            operation: Operation::Close,
        })
        .unwrap();
        let frame = [(body.len() as u32).to_be_bytes().as_slice(), &body].concat();
        c.write_all(&frame[..2]).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        flow.write_all(b"GET / HTTP/1.1\r\nHost: api.example\r\n\r\n")
            .await
            .unwrap();
        flow.shutdown().await.unwrap();
        let mut byte = [0];
        assert!(matches!(flow.read(&mut byte).await, Ok(0) | Err(_)));
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        c.write_all(&frame[2..]).await.unwrap();
        assert_eq!(reply(&mut c).await.state, "closed");
        drop(c);
        task.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn policy_apply_waits_for_helper_cleanup_eof() {
        let f = Fixture::new().await;
        let state = (*f.registry.lookup_session(f.allocation.session_id).unwrap()).clone();
        let (mut c, task) = f.start(f.allocation.clone());
        reply(&mut c).await;
        let registry = f.registry.clone();
        let apply = tokio::spawn(async move { registry.apply(state).await });
        let mut byte = [0];
        assert_eq!(c.read(&mut byte).await.unwrap(), 0);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(
            !apply.is_finished(),
            "write-half EOF is a stop request, not the helper cleanup barrier"
        );
        assert!(f.registry.lookup_session(f.allocation.session_id).is_none());
        drop(c);
        task.await.unwrap().unwrap();
        apply.await.unwrap().unwrap();
        assert!(f.registry.lookup_session(f.allocation.session_id).is_some());
    }

    #[tokio::test]
    async fn missing_helper_barrier_poison_is_session_local_and_cannot_be_replayed() {
        let f = Fixture::new().await;
        let state = (*f.registry.lookup_session(f.allocation.session_id).unwrap()).clone();
        let (mut c, task) = f.start(f.allocation.clone());
        reply(&mut c).await;
        let registry = f.registry.clone();
        let old = state.clone();
        let apply = tokio::spawn(async move { registry.apply(old).await });
        let mut byte = [0];
        assert_eq!(c.read(&mut byte).await.unwrap(), 0);
        // Keep the helper write half open. Another session must remain usable
        // during this allocation's bounded cleanup wait.
        let mut other = state.clone();
        other.session_id = SessionId::new();
        other.sandbox_id = SandboxId::new();
        other.guest_ip = "192.0.2.8".parse().unwrap();
        f.registry.apply(other.clone()).await.unwrap();
        assert!(f.registry.enroll_session(other.session_id).is_some());
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(6), apply)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(task.await.unwrap().is_err());
        assert!(f.registry.is_poisoned(state.session_id));
        f.registry.unregister(state.session_id);
        assert!(f.registry.apply(state.clone()).await.is_err());
        assert!(f.registry.enroll_session(state.session_id).is_none());
        let mut next = f.allocation.clone();
        next.attachment_generation += 1;
        let (mut rejected, next_task) = f.start(next);
        assert!(wire::read::<_, Reply>(&mut rejected).await.is_err());
        assert!(next_task.await.unwrap().is_err());
        assert!(f.registry.enroll_session(other.session_id).is_some());
        drop(c);
    }
    #[tokio::test]
    async fn policy_quarantine_joins_allocation_and_dns_without_global_block() {
        let f = Fixture::new().await;
        let (mut c, task) = f.start(f.allocation.clone());
        reply(&mut c).await;
        request(&mut c, 1, Operation::Bind).await;
        reply(&mut c).await;
        request(
            &mut c,
            2,
            Operation::Dns {
                destination: f.allocation.gateway_ip,
                port: 53,
            },
        )
        .await;
        let mut flow = handoff(&c).await;
        reply(&mut c).await;
        flow.write_all(&[0, 128, 0]).await.unwrap();
        let closed = tokio::spawn(async move {
            assert!(wire::read::<_, Reply>(&mut c).await.is_err());
            drop(c);
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            f.registry.quarantine_and_drain(f.allocation.session_id),
        )
        .await
        .unwrap();
        closed.await.unwrap();
        let mut b = [0];
        assert_eq!(flow.read(&mut b).await.unwrap(), 0);
        task.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn wrong_identity_epoch_replay_and_non_tls_fail_closed() {
        let f = Fixture::new().await;
        for kind in 0..5 {
            let mut bad = f.allocation.clone();
            bad.attachment_generation += kind + 1;
            match kind {
                0 => bad.host_id = HostId::new(),
                1 => bad.proxy_incarnation = "proxy-old".into(),
                2 => bad.sandbox_id = SandboxId::new(),
                3 => bad.policy_revision += 1,
                _ => bad.session_id = SessionId::new(),
            }
            let (mut c, task) = f.start(bad);
            assert!(wire::read::<_, Reply>(&mut c).await.is_err());
            assert!(task.await.unwrap().is_err());
        }
        let mut a = f.allocation.clone();
        a.attachment_generation = 10;
        let (mut c, task) = f.start(a);
        reply(&mut c).await;
        request(&mut c, 1, Operation::Bind).await;
        reply(&mut c).await;
        request(
            &mut c,
            2,
            Operation::Tcp {
                destination: "198.51.100.7".parse().unwrap(),
                port: 80,
            },
        )
        .await;
        let mut flow = handoff(&c).await;
        reply(&mut c).await;
        flow.write_all(b"GET / HTTP/1.1\r\nHost: mcp.example\r\n\r\n")
            .await
            .unwrap();
        let mut bytes = [0; 64];
        assert!(matches!(flow.read(&mut bytes).await, Ok(0) | Err(_)));
        request(&mut c, 2, Operation::Bind).await;
        assert!(wire::read::<_, Reply>(&mut c).await.is_err());
        drop(c);
        assert!(task.await.unwrap().is_err());
    }
    #[tokio::test]
    async fn private_tls_uses_deny_first_and_original_port_authority_without_credentials() {
        let exercise = async {
            for port in [443, 8443] {
                for (host, authority, allowed) in [
                    ("mcp.example", "mcp.example", false),
                    ("api.example", "mcp.example", false),
                    ("api.example", "api.example", true),
                ] {
                    let mut f = Fixture::new().await;
                    let key = rcgen::KeyPair::generate().unwrap();
                    let cert = rcgen::CertificateParams::new(vec!["api.example".into()])
                        .unwrap()
                        .self_signed(&key)
                        .unwrap();
                    let mut roots = rustls::RootCertStore::empty();
                    roots.add(cert.der().clone()).unwrap();
                    let server = rustls::ServerConfig::builder()
                        .with_no_client_auth()
                        .with_single_cert(
                            vec![cert.der().clone()],
                            rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der())
                                .unwrap(),
                        )
                        .unwrap();
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let mut cfg = ProxyConfig::new(
                        "127.0.0.1:0".parse().unwrap(),
                        f.registry.clone(),
                        Arc::new(CertMint::new(f.ca.clone())),
                    );
                    cfg.upstream_test_roots = Some(roots);
                    cfg.resolver = Arc::new(
                        engram_egress_proxy::StaticResolver::new()
                            .with("api.example", listener.local_addr().unwrap()),
                    );
                    f.proxy = Arc::new(Proxy::new(cfg));
                    let (captured, mut received) = tokio::sync::mpsc::channel(1);
                    let upstream = tokio::spawn(async move {
                        let (socket, _) = listener.accept().await.unwrap();
                        let mut tls = tokio_rustls::TlsAcceptor::from(Arc::new(server))
                            .accept(socket)
                            .await
                            .unwrap();
                        let mut bytes = vec![];
                        let mut byte = [0];
                        while tls.read(&mut byte).await.unwrap_or(0) > 0 {
                            bytes.push(byte[0]);
                            if bytes.ends_with(b"\r\n\r\n") {
                                break;
                            }
                        }
                        captured.send(bytes).await.unwrap();
                        let _ = tls.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await;
                        let _ = tls.shutdown().await;
                    });
                    let (mut control, task) = f.start(f.allocation.clone());
                    reply(&mut control).await;
                    request(&mut control, 1, Operation::Bind).await;
                    reply(&mut control).await;
                    request(
                        &mut control,
                        2,
                        Operation::Tcp {
                            destination: "198.51.100.7".parse().unwrap(),
                            port,
                        },
                    )
                    .await;
                    let flow = handoff(&control).await;
                    reply(&mut control).await;
                    let mut trust = rustls::RootCertStore::empty();
                    for der in rustls_pemfile::certs(&mut f.ca.cert_pem.as_bytes()) {
                        trust.add(der.unwrap()).unwrap();
                    }
                    let connector = tokio_rustls::TlsConnector::from(Arc::new(
                        rustls::ClientConfig::builder()
                            .with_root_certificates(trust)
                            .with_no_client_auth(),
                    ));
                    let guest = connector
                        .connect(host.to_string().try_into().unwrap(), flow)
                        .await;
                    if host == "mcp.example" {
                        assert!(guest.is_err());
                    } else {
                        let mut guest = guest.unwrap();
                        let effective = if port == 443 {
                            authority.to_string()
                        } else {
                            format!("{authority}:{port}")
                        };
                        guest.write_all(format!("GET / HTTP/1.1\r\nHost: {effective}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                        let mut out = vec![];
                        let _ = guest.read_to_end(&mut out).await;
                        assert_eq!(out.starts_with(b"HTTP/1.1 200"), allowed);
                        let bytes = received.recv().await.unwrap();
                        if allowed {
                            let text = String::from_utf8(bytes).unwrap();
                            assert!(text.contains(&format!("Host: {effective}\r\n")));
                            assert!(!text.to_lowercase().contains("authorization"));
                        } else {
                            assert!(bytes.is_empty());
                        }
                    }
                    drop(control);
                    task.await.unwrap().unwrap();
                    upstream.abort();
                    let _ = upstream.await;
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), exercise)
            .await
            .expect("bounded real private TLS fixture");
    }

    #[tokio::test]
    async fn unused_older_attachment_generation_cannot_replace_newer() {
        let f = Fixture::new().await;
        let mut newer = f.allocation.clone();
        newer.attachment_generation = 5;
        let (mut current, task) = f.start(newer);
        reply(&mut current).await;
        let (mut older, replay) = f.start(f.allocation.clone());
        assert!(wire::read::<_, Reply>(&mut older).await.is_err());
        assert!(replay.await.unwrap().is_err());
        request(&mut current, 1, Operation::Bind).await;
        assert_eq!(reply(&mut current).await.state, "ready");
        drop(current);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn private_dns_uses_controlled_resolver_and_joins_upstream_on_revocation() {
        for return_answer in [true, false] {
            let mut f = Fixture::new().await;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut cfg = ProxyConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                f.registry.clone(),
                Arc::new(CertMint::new(f.ca.clone())),
            );
            cfg.dns_upstream = listener.local_addr().unwrap();
            f.proxy = Arc::new(Proxy::new(cfg));
            let query = b"\x00\x07\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x03api\x07example\x00\x00\x01\x00\x01";
            let (seen, received) = tokio::sync::oneshot::channel();
            let upstream = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let length = socket.read_u16().await.unwrap();
                let mut data = vec![0; length as usize];
                socket.read_exact(&mut data).await.unwrap();
                assert_eq!(data, query);
                seen.send(()).unwrap();
                if return_answer {
                    data[2] |= 0x80;
                    socket.write_u16(length).await.unwrap();
                    socket.write_all(&data).await.unwrap();
                } else {
                    let mut byte = [0];
                    assert_eq!(
                        socket.read(&mut byte).await.unwrap(),
                        0,
                        "revocation must close the controlled upstream"
                    );
                }
            });
            let (mut c, task) = f.start(f.allocation.clone());
            reply(&mut c).await;
            request(&mut c, 1, Operation::Bind).await;
            reply(&mut c).await;
            request(
                &mut c,
                2,
                Operation::Dns {
                    destination: f.allocation.gateway_ip,
                    port: 53,
                },
            )
            .await;
            let mut dns = handoff(&c).await;
            reply(&mut c).await;
            dns.write_u16(query.len() as u16).await.unwrap();
            dns.write_all(query).await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(1), received)
                .await
                .unwrap()
                .unwrap();
            if return_answer {
                let length = dns.read_u16().await.unwrap();
                let mut response = vec![0; length as usize];
                dns.read_exact(&mut response).await.unwrap();
                let mut expected = query.to_vec();
                expected[2] |= 0x80;
                assert_eq!(response, expected);
            }
            let helper = tokio::spawn(async move {
                assert!(wire::read::<_, Reply>(&mut c).await.is_err());
                drop(c);
            });
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                f.registry.quarantine_and_drain(f.allocation.session_id),
            )
            .await
            .unwrap();
            helper.await.unwrap();
            upstream.await.unwrap();
            task.await.unwrap().unwrap();
        }
    }
    #[tokio::test]
    async fn two_allocations_keep_dns_identity_and_revocation_is_local() {
        let f = Fixture::new().await;
        let mut other = (*f.registry.lookup_session(f.allocation.session_id).unwrap()).clone();
        other.session_id = SessionId::new();
        other.sandbox_id = SandboxId::new();
        other.guest_ip = "192.0.2.6".parse().unwrap();
        other.managed_deny = Some(ManagedDenyPolicy::new(1, ["api.example".into()]).unwrap());
        let mut a = f.allocation.clone();
        a.session_id = other.session_id;
        a.sandbox_id = other.sandbox_id;
        a.guest_ip = other.guest_ip;
        a.gateway_ip = "192.0.2.5".parse().unwrap();
        a.boot_attempt = "boot-2".into();
        a.policy_digest = other.managed_deny.as_ref().unwrap().digest.clone();
        f.registry.apply(other).await.unwrap();
        let (mut first, one) = f.start(f.allocation.clone());
        reply(&mut first).await;
        let (mut second, two) = f.start(a.clone());
        reply(&mut second).await;
        request(&mut second, 1, Operation::Bind).await;
        reply(&mut second).await;
        let first_closed = tokio::spawn(async move {
            assert!(wire::read::<_, Reply>(&mut first).await.is_err());
            drop(first);
        });
        f.registry
            .quarantine_and_drain(f.allocation.session_id)
            .await;
        first_closed.await.unwrap();
        one.await.unwrap().unwrap();
        request(
            &mut second,
            2,
            Operation::Dns {
                destination: a.gateway_ip,
                port: 53,
            },
        )
        .await;
        let mut dns = handoff(&second).await;
        reply(&mut second).await;
        let query=b"\x00\x07\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\x03api\x07example\x00\x00\x01\x00\x01";
        dns.write_u16(query.len() as u16).await.unwrap();
        dns.write_all(query).await.unwrap();
        let size = dns.read_u16().await.unwrap();
        let mut answer = vec![0; size as usize];
        dns.read_exact(&mut answer).await.unwrap();
        assert_eq!(
            answer[3] & 15,
            3,
            "the second allocation denies api.example independently"
        );
        request(&mut second, 3, Operation::Close).await;
        assert_eq!(reply(&mut second).await.state, "closed");
        drop(second);
        two.await.unwrap().unwrap();
    }
}
