//! Bounded TCP/DNS streams reach only the registered private policy ingress.
use engram_egress_proto::private::{self as wire, Operation, Reply, Request};
use std::{
    io,
    net::{Ipv4Addr, SocketAddrV4},
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket, UnixStream},
    sync::{mpsc, oneshot, watch, Mutex},
    task::JoinSet,
};

const CLOSED: u8 = 0;
const READY: u8 = 1;
const DEAD: u8 = 2;

pub(super) struct Adapter {
    cancel: watch::Sender<bool>,
    // Only the later durable start implementation may expose a consuming
    // release operation. Holding this sender keeps the worker dormant.
    _release: Option<oneshot::Sender<u64>>,
    state: Arc<AtomicU8>,
    done: oneshot::Receiver<()>,
    result: Option<Result<(), String>>,
    probe: Arc<std::sync::Mutex<Option<std::os::unix::net::UnixStream>>>,
}
impl Adapter {
    pub fn spawn(
        private: UnixStream,
        tcp: TcpListener,
        dns: UdpSocket,
        guest: Ipv4Addr,
        gateway: Ipv4Addr,
        terminal: Option<std::sync::mpsc::Sender<()>>,
    ) -> io::Result<Self> {
        use std::os::fd::AsFd;
        let probe = Arc::new(std::sync::Mutex::new(Some(
            std::os::unix::net::UnixStream::from(private.as_fd().try_clone_to_owned()?),
        )));
        let worker_probe = probe.clone();
        let (cancel, mut stopped) = watch::channel(false);
        let (release, released) = oneshot::channel();
        let (done_tx, done) = oneshot::channel();
        let state = Arc::new(AtomicU8::new(CLOSED));
        let state_worker = state.clone();
        let lifetime = Lifetime {
            state: state.clone(),
            probe: worker_probe,
            terminal,
            reader: None,
        };
        tokio::spawn(async move {
            let mut lifetime = lifetime;
            let (read, write) = private.into_split();
            let (reply_tx, replies) = mpsc::channel(1);
            let (dead_tx, mut dead) = watch::channel(false);
            let fail = dead_tx.clone();
            let reader_state = state_worker.clone();
            let mut reader = tokio::spawn(async move {
                let mut read = read;
                loop {
                    // Any bytes/EOF while CLOSED are terminal, including a
                    // partial header. Do not spend a handoff timeout parsing it.
                    if read.as_ref().readable().await.is_err() {
                        break;
                    }
                    if reader_state.load(Ordering::Acquire) == CLOSED {
                        break;
                    }
                    let Ok(reply) = wire::read::<_, Reply>(&mut read).await else {
                        break;
                    };
                    if reply_tx.send(reply).await.is_err() {
                        break;
                    }
                }
                dead_tx.send_replace(true);
            });
            lifetime.reader = Some(reader.abort_handle());
            let proxy = Arc::new(Proxy {
                call: Mutex::new(Call {
                    write,
                    replies,
                    sequence: 0,
                    epoch: 0,
                    failed: false,
                }),
                fail,
            });
            let mut flows = JoinSet::new();
            let result = tokio::select! { biased;
                _ = stopped.changed() => Ok(()),
                _ = dead.changed() => Err(wire::invalid()),
                result = async {
                    let epoch = released.await.map_err(|_| wire::invalid())?;
                    if epoch == 0 || epoch > 9_007_199_254_740_991 { return Err(wire::invalid()); }
                    state_worker.store(3, Ordering::Release);
                    proxy.call.lock().await.epoch = epoch;
                    proxy.request(Operation::Bind, None).await?;
                    state_worker.store(READY, Ordering::Release);
                    serve(&tcp, &dns, guest, gateway, proxy.clone(), &mut flows).await
                } => result,
            };
            let _ = result;
            state_worker.store(DEAD, Ordering::Release);
            if let Some(signal) = &lifetime.terminal {
                let _ = signal.send(());
            }
            // Listeners and every accepted guest/Unix socket close before the
            // private write half returns cleanup EOF to the policy lease.
            drop(tcp);
            drop(dns);
            flows.shutdown().await;
            reader.abort();
            let _ = (&mut reader).await;
            lifetime.probe.lock().expect("probe lock").take();
            drop(proxy);
            let _ = done_tx.send(());
        });
        Ok(Self {
            cancel,
            _release: Some(release),
            state,
            done,
            result: None,
            probe,
        })
    }
    pub fn check_closed(&self) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        if self.state.load(Ordering::Acquire) != CLOSED {
            return Err(io::Error::other("private network is no longer closed"));
        }
        let guard = self.probe.lock().map_err(|_| wire::invalid())?;
        let socket = guard.as_ref().ok_or_else(wire::invalid)?;
        let mut byte = [0u8];
        // SAFETY: live owned stream and one writable byte. PEEK cannot consume
        // a reply; no reply is legal before the private release command.
        let count = unsafe {
            libc::recv(
                socket.as_raw_fd(),
                byte.as_mut_ptr().cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if count < 0 && io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock {
            return Ok(());
        }
        self.state.store(DEAD, Ordering::Release);
        self.cancel.send_replace(true);
        Err(io::Error::other("private network control stopped"))
    }
    pub async fn close(&mut self) -> io::Result<()> {
        self.cancel.send_replace(true);
        if let Some(result) = &self.result {
            return result.clone().map_err(io::Error::other);
        }
        let result =
            tokio::time::timeout(Duration::from_secs(wire::HANDOFF_SECONDS), &mut self.done)
                .await
                .map_err(|_| io::Error::other("network cleanup timed out"))
                .and_then(|r| r.map_err(|_| io::Error::other("network cleanup stopped")));
        self.result = Some(result.as_ref().map(|_| ()).map_err(ToString::to_string));
        result
    }
}
impl Drop for Adapter {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
    }
}
// This guard is constructed before spawn. Runtime cancellation, including an
// unpolled task, cannot leave a live-looking closed owner or a private probe FD.
struct Lifetime {
    state: Arc<AtomicU8>,
    probe: Arc<std::sync::Mutex<Option<std::os::unix::net::UnixStream>>>,
    terminal: Option<std::sync::mpsc::Sender<()>>,
    reader: Option<tokio::task::AbortHandle>,
}
impl Drop for Lifetime {
    fn drop(&mut self) {
        self.state.store(DEAD, Ordering::Release);
        if let Some(probe) = self.probe.lock().expect("probe lock").take() {
            // Terminal shutdown applies to all duplicates of this control
            // socket, even if another task has not yet dropped its half.
            let _ = probe.shutdown(std::net::Shutdown::Both);
        }
        if let Some(reader) = &self.reader {
            reader.abort();
        }
        if let Some(signal) = &self.terminal {
            let _ = signal.send(());
        }
    }
}

struct Call {
    write: tokio::net::unix::OwnedWriteHalf,
    replies: mpsc::Receiver<Reply>,
    sequence: u64,
    epoch: u64,
    failed: bool,
}
struct Proxy {
    call: Mutex<Call>,
    fail: watch::Sender<bool>,
}
impl Proxy {
    async fn request(&self, operation: Operation, fd: Option<&UnixStream>) -> io::Result<()> {
        use std::os::fd::AsFd;
        let mut call = self.call.lock().await;
        if call.failed {
            return Err(wire::invalid());
        }
        call.sequence = call.sequence.checked_add(1).ok_or_else(wire::invalid)?;
        let expected = Reply {
            version: wire::VERSION,
            sequence: call.sequence,
            epoch: call.epoch,
            state: if matches!(operation, Operation::Bind) {
                "ready"
            } else {
                "accepted"
            }
            .into(),
        };
        let request = Request {
            version: wire::VERSION,
            sequence: call.sequence,
            epoch: call.epoch,
            operation,
        };
        let result = tokio::time::timeout(Duration::from_secs(wire::HANDOFF_SECONDS), async {
            wire::write(&mut call.write, &request).await?;
            if let Some(fd) = fd {
                let socket = call.write.as_ref();
                let carrier =
                    std::os::unix::net::UnixStream::from(socket.as_fd().try_clone_to_owned()?);
                socket
                    .async_io(tokio::io::Interest::WRITABLE, || {
                        engram_egress_proto::send_fd(&carrier, fd.as_fd())
                    })
                    .await?;
            }
            if call.replies.recv().await != Some(expected) {
                return Err(wire::invalid());
            }
            Ok(())
        })
        .await
        .map_err(|_| wire::invalid())
        .and_then(|r| r);
        if result.is_err() {
            call.failed = true;
            self.fail.send_replace(true);
        }
        result
    }
    async fn flow(&self, operation: Operation) -> io::Result<UnixStream> {
        let (local, remote) = UnixStream::pair()?;
        self.request(operation, Some(&remote)).await?;
        Ok(local)
    }
}

async fn serve(
    tcp: &TcpListener,
    dns: &UdpSocket,
    guest: Ipv4Addr,
    gateway: Ipv4Addr,
    proxy: Arc<Proxy>,
    flows: &mut JoinSet<io::Result<()>>,
) -> io::Result<()> {
    let dns_budget = Arc::new(tokio::sync::Semaphore::new(wire::MAX_DNS_FLOWS));
    let mut bytes = [0; wire::MAX_DNS + 1];
    loop {
        tokio::select! { biased;
            Some(result) = flows.join_next(), if !flows.is_empty() => { result.map_err(|_| wire::invalid())??; }
            connection = tcp.accept() => {
                let (stream, peer) = connection?;
                if peer.ip() != guest || flows.len() >= wire::MAX_FLOWS { continue; }
                let destination = original_destination(&stream)?;
                let is_dns = *destination.ip() == gateway && destination.port() == 53;
                let permit = if is_dns { match dns_budget.clone().try_acquire_owned() { Ok(p) => Some(p), Err(_) => continue } } else { None };
                let proxy = proxy.clone();
                flows.spawn(async move {
                    let _permit = permit;
                    tcp_flow(stream, destination, gateway, proxy).await
                });
            }
            datagram = dns.recv_from(&mut bytes) => {
                let (len, peer) = datagram?;
                if peer.ip() != guest || len == 0 || len > wire::MAX_DNS || flows.len() >= wire::MAX_FLOWS { continue; }
                let Ok(permit) = dns_budget.clone().try_acquire_owned() else { continue; };
                let query = bytes[..len].to_vec();
                let proxy = proxy.clone();
                // A duplicate of the same namespace-bound socket, never a new
                // root-namespace socket or a dial to an upstream destination.
                use std::os::fd::AsFd;
                let socket = UdpSocket::from_std(std::net::UdpSocket::from(dns.as_fd().try_clone_to_owned()?))?;
                flows.spawn(async move {
                    let _permit = permit;
                    match dns_exchange(query, gateway, proxy).await {
                        Ok(reply) => { socket.send_to(&reply, peer).await?; },
                        Err(_) => return Ok(()),
                    }
                    Ok(())
                });
            }
        }
    }
}
async fn dns_exchange(query: Vec<u8>, gateway: Ipv4Addr, proxy: Arc<Proxy>) -> io::Result<Vec<u8>> {
    tokio::time::timeout(Duration::from_secs(10), async {
        if query.is_empty() || query.len() > wire::MAX_DNS {
            return Err(wire::invalid());
        }
        let mut stream = proxy
            .flow(Operation::Dns {
                destination: gateway,
                port: 53,
            })
            .await?;
        stream.write_u16(query.len() as u16).await?;
        stream.write_all(&query).await?;
        let len = stream.read_u16().await? as usize;
        if len == 0 || len > wire::MAX_DNS {
            return Err(wire::invalid());
        }
        let mut reply = vec![0; len];
        stream.read_exact(&mut reply).await?;
        Ok(reply)
    })
    .await
    .map_err(|_| wire::invalid())?
}
async fn tcp_flow(
    mut stream: TcpStream,
    destination: SocketAddrV4,
    gateway: Ipv4Addr,
    proxy: Arc<Proxy>,
) -> io::Result<()> {
    if *destination.ip() == gateway && destination.port() == 53 {
        // One bounded DNS request per TCP connection. The reviewed proxy owns
        // DNS decoding and policy; no raw TCP tunnel reaches its resolver.
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            let len = stream.read_u16().await? as usize;
            if len == 0 || len > wire::MAX_DNS {
                return Err(wire::invalid());
            }
            let mut query = vec![0; len];
            stream.read_exact(&mut query).await?;
            let reply = dns_exchange(query, gateway, proxy).await?;
            stream.write_u16(reply.len() as u16).await?;
            stream.write_all(&reply).await
        })
        .await;
        let _ = result;
        return Ok(());
    }
    let mut upstream = proxy
        .flow(Operation::Tcp {
            destination: *destination.ip(),
            port: destination.port(),
        })
        .await?;
    let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
    Ok(())
}
fn checked_destination(address: Ipv4Addr, port: u16) -> io::Result<SocketAddrV4> {
    if port == 0 || address.is_unspecified() || address.is_multicast() {
        return Err(wire::invalid());
    }
    Ok(SocketAddrV4::new(address, port))
}
#[cfg(target_os = "linux")]
fn original_destination(stream: &TcpStream) -> io::Result<SocketAddrV4> {
    use std::os::fd::AsRawFd;
    let mut address: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of_val(&address) as libc::socklen_t;
    // SAFETY: a live socket, correct sockaddr size, and writable output. The
    // kernel tuple is mandatory; no local address, SNI port or 443 fallback.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_IP,
            80,
            (&mut address as *mut libc::sockaddr_in).cast(),
            &mut len,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if len as usize != std::mem::size_of_val(&address)
        || address.sin_family != libc::AF_INET as libc::sa_family_t
    {
        return Err(wire::invalid());
    }
    checked_destination(
        Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes()),
        u16::from_be(address.sin_port),
    )
}
#[cfg(not(target_os = "linux"))]
fn original_destination(_stream: &TcpStream) -> io::Result<SocketAddrV4> {
    Err(io::Error::other("original destination requires Linux"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn closed_adapter_emits_nothing_and_proxy_loss_is_terminal() {
        let (private, mut peer) = UnixStream::pair().unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = tcp.local_addr().unwrap();
        let dns = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut adapter = Adapter::spawn(
            private,
            tcp,
            dns,
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::LOCALHOST,
            None,
        )
        .unwrap();
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(b"untrusted").await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), peer.read_u8())
                .await
                .is_err()
        );
        peer.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_millis(200), async {
            loop {
                if adapter.check_closed().is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
        assert!(adapter.check_closed().is_err());
        // The worker has already completed; close can still join that result.
        adapter.close().await.unwrap();
    }
    #[tokio::test]
    async fn repeated_close_preserves_cleanup_result() {
        let (private, _peer) = UnixStream::pair().unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dns = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut adapter = Adapter::spawn(
            private,
            tcp,
            dns,
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::LOCALHOST,
            None,
        )
        .unwrap();
        adapter.close().await.unwrap();
        adapter.close().await.unwrap();
        assert!(adapter.check_closed().is_err());
    }
    #[tokio::test]
    async fn dns_budget_and_stop_join_all_owned_flows_before_proxy_eof() {
        let (private, mut peer) = UnixStream::pair().unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dns = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = dns.local_addr().unwrap();
        let mut adapter = Adapter::spawn(
            private,
            tcp,
            dns,
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::LOCALHOST,
            None,
        )
        .unwrap();
        adapter._release.take().unwrap().send(7).unwrap();
        let bind: Request = wire::read(&mut peer).await.unwrap();
        assert!(matches!(bind.operation, Operation::Bind));
        wire::write(
            &mut peer,
            &Reply {
                version: 1,
                sequence: 1,
                epoch: 7,
                state: "ready".into(),
            },
        )
        .await
        .unwrap();
        let guest = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut handed = Vec::new();
        for i in 0..wire::MAX_DNS_FLOWS {
            guest.send_to(b"query", address).await.unwrap();
            let request: Request = wire::read(&mut peer).await.unwrap();
            assert_eq!(request.sequence, i as u64 + 2);
            let mut stream = wire::receive_stream(&peer).await.unwrap();
            wire::write(
                &mut peer,
                &Reply {
                    version: 1,
                    sequence: request.sequence,
                    epoch: 7,
                    state: "accepted".into(),
                },
            )
            .await
            .unwrap();
            assert_eq!(stream.read_u16().await.unwrap(), 5);
            let mut query = [0; 5];
            stream.read_exact(&mut query).await.unwrap();
            assert_eq!(&query, b"query");
            handed.push(stream);
        }
        guest.send_to(b"ninth", address).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), peer.read_u8())
                .await
                .is_err()
        );
        peer.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_millis(200), async {
            assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
            for mut stream in handed {
                assert_eq!(stream.read(&mut [0]).await.unwrap(), 0);
            }
        })
        .await
        .unwrap();
        adapter.close().await.unwrap();
    }
    #[test]
    fn runtime_loss_invalidates_even_unpolled_owners_and_closes_private_probe() {
        use std::io::Read;
        for poll_once in [false, true] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (terminal, stopped) = std::sync::mpsc::channel();
            let (adapter, mut peer, retained_duplicate) = runtime.block_on(async {
                let (private, peer) = UnixStream::pair().unwrap();
                use std::os::fd::AsFd;
                // Hold a duplicate past runtime destruction. Terminal control
                // must not depend on the order of independent FD destructors.
                let duplicate = private.as_fd().try_clone_to_owned().unwrap();
                let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let dns = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                let adapter = Adapter::spawn(
                    private,
                    tcp,
                    dns,
                    Ipv4Addr::LOCALHOST,
                    Ipv4Addr::LOCALHOST,
                    Some(terminal),
                )
                .unwrap();
                if poll_once {
                    tokio::task::yield_now().await;
                }
                (adapter, peer.into_std().unwrap(), duplicate)
            });
            drop(runtime);
            assert!(adapter.check_closed().is_err());
            stopped.recv_timeout(Duration::from_millis(100)).unwrap();
            assert_eq!(peer.read(&mut [0]).unwrap(), 0);
            drop(retained_duplicate);
        }
    }
    #[tokio::test]
    async fn partial_header_is_terminal_before_frame_timeout() {
        let (private, mut peer) = UnixStream::pair().unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dns = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let adapter = Adapter::spawn(
            private,
            tcp,
            dns,
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::LOCALHOST,
            None,
        )
        .unwrap();
        tokio::task::yield_now().await;
        peer.write_all(&[0]).await.unwrap();
        tokio::time::timeout(Duration::from_millis(100), async {
            loop {
                if adapter.check_closed().is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
            assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
        })
        .await
        .unwrap();
    }
    fn fixture_proxy(stream: UnixStream) -> (Arc<Proxy>, tokio::task::JoinHandle<()>) {
        let (mut read, write) = stream.into_split();
        let (tx, replies) = mpsc::channel(1);
        let (fail, _) = watch::channel(false);
        let reader = tokio::spawn(async move {
            while let Ok(reply) = wire::read::<_, Reply>(&mut read).await {
                if tx.send(reply).await.is_err() {
                    break;
                }
            }
        });
        (
            Arc::new(Proxy {
                call: Mutex::new(Call {
                    write,
                    replies,
                    sequence: 0,
                    epoch: 7,
                    failed: false,
                }),
                fail,
            }),
            reader,
        )
    }
    #[tokio::test]
    async fn tcp_bridge_preserves_original_nonstandard_port_and_transfers_only_private_stream() {
        let (private, mut peer) = UnixStream::pair().unwrap();
        let (proxy, reader) = fixture_proxy(private);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut guest = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let bridge = tokio::spawn(tcp_flow(
            stream,
            "198.51.100.9:8443".parse().unwrap(),
            "192.0.2.1".parse().unwrap(),
            proxy,
        ));
        let request: Request = wire::read(&mut peer).await.unwrap();
        assert_eq!((request.sequence, request.epoch), (1, 7));
        assert!(
            matches!(request.operation, Operation::Tcp { destination, port: 8443 } if destination.to_string() == "198.51.100.9")
        );
        let mut handed = wire::receive_stream(&peer).await.unwrap();
        wire::write(
            &mut peer,
            &Reply {
                version: 1,
                sequence: 1,
                epoch: 7,
                state: "accepted".into(),
            },
        )
        .await
        .unwrap();
        guest.write_all(b"guest-bytes").await.unwrap();
        let mut bytes = [0; 11];
        handed.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"guest-bytes");
        handed.write_all(b"reply").await.unwrap();
        let mut bytes = [0; 5];
        guest.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"reply");
        guest.shutdown().await.unwrap();
        handed.shutdown().await.unwrap();
        bridge.await.unwrap().unwrap();
        reader.abort();
    }
    #[tokio::test]
    async fn dns_is_length_bounded_and_handoff_mismatch_is_terminal() {
        let (private, mut peer) = UnixStream::pair().unwrap();
        let (proxy, reader) = fixture_proxy(private);
        assert!(dns_exchange(
            vec![0; wire::MAX_DNS + 1],
            "192.0.2.1".parse().unwrap(),
            proxy.clone()
        )
        .await
        .is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), peer.read_u8())
                .await
                .is_err()
        );
        let flow = tokio::spawn({
            let proxy = proxy.clone();
            async move { dns_exchange(b"query".to_vec(), "192.0.2.1".parse().unwrap(), proxy).await }
        });
        let request: Request = wire::read(&mut peer).await.unwrap();
        assert!(
            matches!(request.operation, Operation::Dns { destination, port: 53 } if destination.to_string() == "192.0.2.1")
        );
        let mut handed = wire::receive_stream(&peer).await.unwrap();
        // The wrong epoch must poison this transport, even for a DNS failure.
        wire::write(
            &mut peer,
            &Reply {
                version: 1,
                sequence: 1,
                epoch: 8,
                state: "accepted".into(),
            },
        )
        .await
        .unwrap();
        assert!(flow.await.unwrap().is_err());
        assert!(*proxy.fail.borrow());
        assert!(proxy
            .flow(Operation::Dns {
                destination: "192.0.2.1".parse().unwrap(),
                port: 53
            })
            .await
            .is_err());
        assert_eq!(handed.read(&mut [0]).await.unwrap(), 0);
        reader.abort();
    }
    #[test]
    fn mandatory_destination_never_defaults_the_port() {
        assert!(checked_destination(Ipv4Addr::UNSPECIFIED, 443).is_err());
        assert!(checked_destination("198.51.100.9".parse().unwrap(), 0).is_err());
        assert_eq!(
            checked_destination("198.51.100.9".parse().unwrap(), 8443)
                .unwrap()
                .port(),
            8443
        );
    }
}
