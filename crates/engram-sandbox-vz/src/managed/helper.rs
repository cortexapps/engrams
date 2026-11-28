//! The native spawn flag closes even unrelated inheritable host descriptors.
use engram_egress_proto::private::{self, Reply};
use std::{
    ffi::CString,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, net::UnixDatagram},
    },
    path::Path,
    time::{Duration, Instant},
};
use tokio::net::UnixStream;

const DEADLINE: Duration = Duration::from_secs(private::HANDOFF_SECONDS);

#[derive(serde::Serialize)]
pub(super) struct Config {
    pub version: u32,
    pub guest: std::net::Ipv4Addr,
    pub gateway: std::net::Ipv4Addr,
    pub mac: String,
}

async fn read_closed(control: &mut UnixStream) -> io::Result<()> {
    let reply: Reply = tokio::time::timeout(DEADLINE, private::read(control))
        .await
        .map_err(|_| private::invalid())??;
    if reply
        != (Reply {
            version: 1,
            sequence: 0,
            epoch: 0,
            state: "closed".into(),
        })
    {
        return Err(private::invalid());
    }
    Ok(())
}

pub(super) struct Helper {
    control: UnixStream,
    child: Option<Child>,
    cleanup: Cleanup,
    cleanup_complete: bool,
    cleanup_failed: bool,
    released_epoch: Option<u64>,
    release_pending: bool,
}
impl Helper {
    pub async fn spawn(
        path: &Path,
        packet: UnixDatagram,
        proxy: UnixStream,
        config: &Config,
    ) -> io::Result<Self> {
        let (control, child_control) = UnixStream::pair()?;
        // Reserve the cleanup thread before creating a process. Drop then needs
        // neither a running Tokio runtime nor a new thread allocation.
        let cleanup = Cleanup::new()?;
        let child = Child::spawn(
            path,
            [
                packet.as_raw_fd(),
                child_control.as_raw_fd(),
                proxy.as_raw_fd(),
            ],
        )?;
        // Only the helper retains the child endpoints after spawn.
        drop((packet, child_control, proxy));
        let mut helper = Self {
            control,
            child: Some(child),
            cleanup,
            cleanup_complete: false,
            cleanup_failed: false,
            released_epoch: None,
            release_pending: false,
        };
        // Establish the Drop owner before the first cancellable handshake write.
        private::write(&mut helper.control, config).await?;
        read_closed(&mut helper.control).await?;
        helper.check_closed()?;
        Ok(helper)
    }
    /// Reliable-channel and process checks. Datagram EOF is never evidence.
    pub fn check_closed(&mut self) -> io::Result<()> {
        if self.released_epoch.is_some() || self.release_pending {
            return Err(private::invalid());
        }
        self.check_live()
    }
    pub fn check_ready(&mut self, epoch: u64) -> io::Result<()> {
        if self.released_epoch != Some(epoch) || self.release_pending {
            return Err(private::invalid());
        }
        self.check_live()
    }
    /// One exact epoch, acknowledged by the helper after its private proxy bind.
    /// A canceled handshake cannot be sent again on this owner.
    pub async fn release(&mut self, epoch: u64) -> io::Result<()> {
        if self.released_epoch == Some(epoch) && !self.release_pending {
            return self.check_ready(epoch);
        }
        self.check_closed()?;
        if epoch == 0 || epoch > 9_007_199_254_740_991 {
            return Err(private::invalid());
        }
        use std::os::fd::AsFd;
        struct ReleaseGuard {
            socket: std::os::unix::net::UnixStream,
            armed: bool,
        }
        impl Drop for ReleaseGuard {
            fn drop(&mut self) {
                if self.armed {
                    let _ = self.socket.shutdown(std::net::Shutdown::Both);
                }
            }
        }
        let mut guard = ReleaseGuard {
            socket: std::os::unix::net::UnixStream::from(
                self.control.as_fd().try_clone_to_owned()?,
            ),
            armed: true,
        };
        self.release_pending = true;
        let result = tokio::time::timeout(DEADLINE, async {
            private::write(
                &mut self.control,
                &serde_json::json!({"version":1,"kind":"open","epoch":epoch}),
            )
            .await?;
            let reply: Reply = private::read(&mut self.control).await?;
            if reply
                != (Reply {
                    version: 1,
                    sequence: 1,
                    epoch,
                    state: "ready".into(),
                })
            {
                return Err(private::invalid());
            }
            Ok(())
        })
        .await
        .map_err(|_| private::invalid())
        .and_then(|r| r);
        if result.is_err() {
            self.begin_close();
            return result;
        }
        guard.armed = false;
        self.released_epoch = Some(epoch);
        self.release_pending = false;
        self.check_ready(epoch)
    }
    fn check_live(&mut self) -> io::Result<()> {
        if self.cleanup_failed
            || self
                .child
                .as_mut()
                .ok_or_else(private::invalid)?
                .poll()?
                .is_some()
        {
            return Err(private::invalid());
        }
        let mut byte = [0];
        // SAFETY: a live nonblocking socket, borrowed buffer, no FD transfer.
        let n = unsafe {
            libc::recv(
                self.control.as_raw_fd(),
                byte.as_mut_ptr().cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock {
            Ok(())
        } else {
            Err(private::invalid())
        }
    }
    fn begin_close(&mut self) {
        // SAFETY: control owns this live socket. Shutdown changes no FD ownership.
        unsafe {
            libc::shutdown(self.control.as_raw_fd(), libc::SHUT_RDWR);
        }
        if let Some(child) = self.child.take() {
            // The reserved worker waits only on this channel before receiving
            // the child; it cannot exit while its sender is still owned here.
            self.cleanup
                .sender
                .take()
                .expect("cleanup sender")
                .send(child)
                .expect("reserved cleanup worker");
        }
    }
    pub async fn close(&mut self) -> io::Result<()> {
        self.begin_close();
        if self.cleanup_failed {
            return Err(io::Error::other("network helper cleanup remains failed"));
        }
        if self.cleanup_complete {
            return Ok(());
        }
        // Keep the receiver in the owner. Canceling this await neither cancels
        // the worker nor consumes its result; Drop does not wait for it.
        let result = (&mut self.cleanup.result)
            .await
            .unwrap_or_else(|_| Err(io::Error::other("network helper cleanup worker failed")));
        self.cleanup_complete = true;
        self.cleanup_failed = result.is_err();
        result
    }
}
impl Drop for Helper {
    fn drop(&mut self) {
        self.begin_close();
    }
}

/// A reserved thread owns the child from cleanup handoff through final reap.
/// Its result is local cleanup evidence only, never a proxy barrier ACK.
struct Cleanup {
    sender: Option<std::sync::mpsc::Sender<Child>>,
    result: tokio::sync::oneshot::Receiver<io::Result<()>>,
}
impl Cleanup {
    fn new() -> io::Result<Self> {
        let (sender, receiver) = std::sync::mpsc::channel::<Child>();
        let (result_tx, result) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("managed-network-cleanup".into())
            .spawn(move || {
                if let Ok(mut child) = receiver.recv() {
                    let result = child.reap_bounded();
                    // Complete all fallback cleanup before publishing a result.
                    drop(child);
                    if let Err(error) = &result {
                        tracing::error!(%error, "network helper cleanup failed");
                    }
                    let _ = result_tx.send(result);
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            result,
        })
    }
}

#[derive(Debug)]
struct Child {
    pid: libc::pid_t,
    status: Option<i32>,
}
impl Child {
    fn spawn(path: &Path, descriptors: [i32; 3]) -> io::Result<Self> {
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| private::invalid())?;
        // Duplicate above the standard slots. This also handles a host whose
        // stdin was closed before socket allocation; dup2 actions cannot alias.
        let mut owned = Vec::new();
        for fd in descriptors {
            // SAFETY: input descriptors are borrowed from live owned sockets.
            let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
            if copy < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fcntl returned a new descriptor owned here.
            owned.push(unsafe { OwnedFd::from_raw_fd(copy) });
        }
        let check = |code| {
            if code == 0 {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(code))
            }
        };
        // SAFETY: initialized POSIX objects are destroyed on every path below.
        // argv has only argv[0], envp is empty. All pointers live through spawn.
        unsafe {
            let mut attr = std::mem::zeroed();
            check(libc::posix_spawnattr_init(&mut attr))?;
            let mut actions = std::mem::zeroed();
            let init = libc::posix_spawn_file_actions_init(&mut actions);
            if init != 0 {
                libc::posix_spawnattr_destroy(&mut attr);
                return Err(io::Error::from_raw_os_error(init));
            }
            let result = (|| {
                check(libc::posix_spawnattr_setflags(
                    &mut attr,
                    libc::POSIX_SPAWN_CLOEXEC_DEFAULT as i16,
                ))?;
                for (slot, fd) in owned.iter().enumerate() {
                    check(libc::posix_spawn_file_actions_adddup2(
                        &mut actions,
                        fd.as_raw_fd(),
                        slot as i32,
                    ))?;
                }
                for fd in &owned {
                    check(libc::posix_spawn_file_actions_addclose(
                        &mut actions,
                        fd.as_raw_fd(),
                    ))?;
                }
                let argv = [path.as_ptr().cast_mut(), std::ptr::null_mut()];
                let envp = [std::ptr::null_mut()];
                let mut pid = 0;
                check(libc::posix_spawn(
                    &mut pid,
                    path.as_ptr(),
                    &actions,
                    &attr,
                    argv.as_ptr(),
                    envp.as_ptr(),
                ))?;
                Ok(Self { pid, status: None })
            })();
            libc::posix_spawn_file_actions_destroy(&mut actions);
            libc::posix_spawnattr_destroy(&mut attr);
            result
        }
    }
    fn poll(&mut self) -> io::Result<Option<i32>> {
        if self.status.is_some() {
            return Ok(self.status);
        }
        let mut status = 0;
        // SAFETY: only this owner waits on the unreaped child PID.
        let result = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
        if result == self.pid {
            self.status = Some(status);
        } else if result < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ECHILD) {
                self.status = Some(-1);
            }
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
        Ok(self.status)
    }
    fn reap_bounded(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline {
            if self.poll()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // Killing an unresponsive child is cleanup, never a successful proxy
        // drain. The proxy owns the stable poison decision and final ACK.
        self.kill_and_reap();
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "network helper cleanup failed",
        ))
    }
    fn kill_and_reap(&mut self) {
        if self.status.is_some() {
            return;
        }
        // SAFETY: an unreaped owned child PID cannot be reused by another process.
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
        }
        loop {
            let mut status = 0;
            // SAFETY: this is the sole owner of this child.
            let result = unsafe { libc::waitpid(self.pid, &mut status, 0) };
            if result == self.pid {
                self.status = Some(status);
                break;
            }
            if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break;
            }
        }
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(flavor = "current_thread")]
    async fn pending_helper_cleanup_does_not_delay_an_unrelated_private_stop() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for await_close in [false, true] {
            let (input, keep_input_open) = std::os::unix::net::UnixStream::pair().unwrap();
            let (control, output) = UnixStream::pair().unwrap();
            let (errors, _read_errors) = UnixStream::pair().unwrap();
            let cleanup = Cleanup::new().unwrap();
            let child = Child::spawn(
                Path::new("/bin/cat"),
                [input.as_raw_fd(), output.as_raw_fd(), errors.as_raw_fd()],
            )
            .unwrap();
            let pid = child.pid;
            drop((input, output, errors));
            let mut helper = Helper {
                control,
                child: Some(child),
                cleanup,
                cleanup_complete: false,
                cleanup_failed: false,
                released_epoch: None,
                release_pending: false,
            };
            let (allocation, mut proxy) = UnixStream::pair().unwrap();
            let started = Instant::now();
            let cleanup = tokio::spawn(async move {
                if await_close {
                    helper.close().await.unwrap();
                } else {
                    drop(helper);
                }
            });
            // First poll must transfer cleanup, not wait for the held child.
            tokio::task::yield_now().await;
            proxy.shutdown().await.unwrap();
            let watcher = tokio::spawn(async move {
                super::super::wait_for_private_stop(&allocation).await;
                drop(allocation);
            });
            assert_eq!(
                tokio::time::timeout(Duration::from_millis(250), proxy.read(&mut [0]))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
            assert!(
                started.elapsed() < Duration::from_millis(500),
                "helper cleanup blocked the unrelated allocation stop"
            );
            // SAFETY: signal zero only observes the owned fixture; it does not reap.
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                0,
                "child must still be held during unrelated cleanup"
            );
            watcher.await.unwrap();
            if await_close {
                assert!(!cleanup.is_finished(), "close must wait for the held child");
                // Cancellation drops the owner but cannot cancel the reserved reaper.
                cleanup.abort();
                assert!(cleanup.await.unwrap_err().is_cancelled());
            } else {
                cleanup.await.unwrap();
            }
            drop(keep_input_open);
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    // SAFETY: signal zero does not change the child or its ownership.
                    if unsafe { libc::kill(pid, 0) } < 0 {
                        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn forced_cleanup_failure_is_terminal_and_child_is_reaped() {
        let (input, _keep_input_open) = std::os::unix::net::UnixStream::pair().unwrap();
        let (control, output) = UnixStream::pair().unwrap();
        let (errors, _read_errors) = UnixStream::pair().unwrap();
        // cat blocks on input and does not implement the reliable close barrier.
        let cleanup = Cleanup::new().unwrap();
        let child = Child::spawn(
            Path::new("/bin/cat"),
            [input.as_raw_fd(), output.as_raw_fd(), errors.as_raw_fd()],
        )
        .unwrap();
        let pid = child.pid;
        drop((input, output, errors));
        let mut helper = Helper {
            control,
            child: Some(child),
            cleanup,
            cleanup_complete: false,
            cleanup_failed: false,
            released_epoch: None,
            release_pending: false,
        };
        assert!(
            tokio::time::timeout(Duration::from_millis(10), helper.close())
                .await
                .is_err()
        );
        assert!(
            helper.check_closed().is_err(),
            "cleanup cannot become ready again"
        );
        assert!(
            helper.close().await.is_err(),
            "unresponsive helper must fail cleanup"
        );
        assert!(
            helper.close().await.is_err(),
            "reaping cannot erase cleanup failure"
        );
        // SAFETY: the owner must have reaped its child after the bounded kill.
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[tokio::test]
    async fn helper_exit_before_ready_closes_private_channel() {
        use tokio::io::AsyncReadExt;
        let (packet, _peer) = UnixDatagram::pair().unwrap();
        let (private, mut proxy) = UnixStream::pair().unwrap();
        let config = Config {
            version: 1,
            guest: "192.0.2.2".parse().unwrap(),
            gateway: "192.0.2.1".parse().unwrap(),
            mac: "02:00:00:00:00:02".into(),
        };
        assert!(
            Helper::spawn(Path::new("/usr/bin/true"), packet, private, &config)
                .await
                .is_err()
        );
        assert_eq!(
            tokio::time::timeout(DEADLINE, proxy.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    #[ignore = "requires the verified packaged native helper"]
    async fn actual_helper_release_ack_and_active_dns_cleanup() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let path = std::path::PathBuf::from(
            std::env::var_os("ENGRAM_SANDBOX_NETSTACK_TEST_BIN").expect("packaged helper"),
        );
        let config = Config {
            version: 1,
            guest: "192.0.2.2".parse().unwrap(),
            gateway: "192.0.2.1".parse().unwrap(),
            mac: "02:00:00:00:00:02".into(),
        };
        let (packet, peer) = UnixDatagram::pair().unwrap();
        let (private, mut proxy) = UnixStream::pair().unwrap();
        let mut helper = Helper::spawn(&path, packet, private, &config)
            .await
            .unwrap();
        let pid = helper.child.as_ref().unwrap().pid;
        use futures::FutureExt;
        let outcome = tokio::time::timeout(
            Duration::from_secs(20),
            std::panic::AssertUnwindSafe(async {
                let ack = tokio::spawn(async move {
                    let bind: private::Request = private::read(&mut proxy).await.unwrap();
                    assert_eq!(bind.epoch, 7);
                    assert_eq!(bind.sequence, 1);
                    assert!(matches!(bind.operation, private::Operation::Bind));
                    private::write(
                        &mut proxy,
                        &Reply {
                            version: 1,
                            sequence: 1,
                            epoch: 7,
                            state: "ready".into(),
                        },
                    )
                    .await
                    .unwrap();
                    proxy
                });
                helper.release(7).await.unwrap();
                let mut proxy = ack.await.unwrap();
                helper.check_ready(7).unwrap();
                helper.release(7).await.unwrap();
                assert!(helper.release(8).await.is_err());
                assert!(helper.check_closed().is_err());
                // One UDP DNS datagram, entirely on the anonymous packet pair. The
                // proxy holds its private stream; there is no host network socket.
                let query = [
                    0, 1, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1, b'a', 7, b'e', b'x', b'a', b'm', b'p',
                    b'l', b'e', 0, 0, 1, 0, 1,
                ];
                let mut frame = vec![0; 14 + 20 + 8 + query.len()];
                frame[..14].copy_from_slice(&[2, 0, 0, 0, 0, 1, 2, 0, 0, 0, 0, 2, 8, 0]);
                let len = (20 + 8 + query.len()) as u16;
                frame[14..34].copy_from_slice(&[
                    0x45,
                    0,
                    (len >> 8) as u8,
                    len as u8,
                    0,
                    1,
                    0,
                    0,
                    64,
                    17,
                    0,
                    0,
                    192,
                    0,
                    2,
                    2,
                    192,
                    0,
                    2,
                    1,
                ]);
                let mut sum: u32 = frame[14..34]
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|b| u32::from(u16::from_be_bytes([b[0], b[1]])))
                    .sum();
                while sum > 65535 {
                    sum = (sum & 65535) + (sum >> 16);
                }
                frame[24..26].copy_from_slice(&(!(sum as u16)).to_be_bytes());
                let udp = (8 + query.len()) as u16;
                frame[34..42].copy_from_slice(&[
                    0x12,
                    0x34,
                    0,
                    53,
                    (udp >> 8) as u8,
                    udp as u8,
                    0,
                    0,
                ]);
                frame[42..].copy_from_slice(&query);
                peer.send(&frame).unwrap();
                let flow: private::Request =
                    tokio::time::timeout(DEADLINE, private::read(&mut proxy))
                        .await
                        .unwrap()
                        .unwrap();
                assert!(matches!(
                    flow.operation,
                    private::Operation::Dns { port: 53, .. }
                ));
                let mut dns = private::receive_stream(&proxy).await.unwrap();
                private::write(
                    &mut proxy,
                    &Reply {
                        version: 1,
                        sequence: flow.sequence,
                        epoch: 7,
                        state: "accepted".into(),
                    },
                )
                .await
                .unwrap();
                assert_eq!(dns.read_u16().await.unwrap() as usize, query.len());
                let mut received = vec![0; query.len()];
                dns.read_exact(&mut received).await.unwrap();
                proxy.shutdown().await.unwrap();
                assert_eq!(
                    tokio::time::timeout(DEADLINE, proxy.read(&mut [0]))
                        .await
                        .unwrap()
                        .unwrap(),
                    0
                );
                assert_eq!(
                    tokio::time::timeout(DEADLINE, dns.read(&mut [0]))
                        .await
                        .unwrap()
                        .unwrap(),
                    0
                );
            })
            .catch_unwind(),
        )
        .await;
        helper.close().await.unwrap();
        outcome
            .expect("bounded helper exercise")
            .expect("helper assertions");
        assert!(helper.check_ready(7).is_err());
        // SAFETY: this test observes only its exact already-reaped child.
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[tokio::test]
    #[ignore = "requires the verified packaged native helper"]
    async fn actual_helper_rejects_wrong_epoch_and_canceled_release() {
        use futures::FutureExt;
        let path = std::path::PathBuf::from(
            std::env::var_os("ENGRAM_SANDBOX_NETSTACK_TEST_BIN").expect("packaged helper"),
        );
        let config = Config {
            version: 1,
            guest: "192.0.2.2".parse().unwrap(),
            gateway: "192.0.2.1".parse().unwrap(),
            mac: "02:00:00:00:00:02".into(),
        };
        for cancel in [false, true] {
            let (packet, _peer) = UnixDatagram::pair().unwrap();
            let (private, mut proxy) = UnixStream::pair().unwrap();
            let mut helper = Helper::spawn(&path, packet, private, &config)
                .await
                .unwrap();
            let pid = helper.child.as_ref().unwrap().pid;
            let outcome = tokio::time::timeout(
                Duration::from_secs(15),
                std::panic::AssertUnwindSafe(async {
                    if cancel {
                        assert!(
                            tokio::time::timeout(Duration::from_millis(20), helper.release(7))
                                .await
                                .is_err()
                        );
                        assert!(helper.release(7).await.is_err());
                    } else {
                        let ack = tokio::spawn(async move {
                            let _: private::Request = private::read(&mut proxy).await.unwrap();
                            private::write(
                                &mut proxy,
                                &Reply {
                                    version: 1,
                                    sequence: 1,
                                    epoch: 8,
                                    state: "ready".into(),
                                },
                            )
                            .await
                            .unwrap();
                            proxy
                        });
                        assert!(helper.release(7).await.is_err());
                        drop(ack.await.unwrap());
                    }
                    assert!(helper.check_ready(7).is_err());
                })
                .catch_unwind(),
            )
            .await;
            let _ = helper.close().await;
            outcome
                .expect("bounded invalid ACK exercise")
                .expect("invalid ACK assertions");
            // SAFETY: observe only the exact child owned and reaped above.
            assert_eq!(
                unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
    }

    // Task 8 supplies the packaged helper and invokes this explicit target.
    // A missing artifact fails; it never turns into a passing skip.
    #[tokio::test]
    #[ignore = "requires ENGRAM_SANDBOX_NETSTACK_TEST_BIN from the pinned helper build"]
    async fn actual_helper_closed_lifetime_and_descriptor_isolation() {
        use tokio::io::AsyncReadExt;
        let path = std::path::PathBuf::from(
            std::env::var_os("ENGRAM_SANDBOX_NETSTACK_TEST_BIN").expect("packaged helper"),
        );
        let config = Config {
            version: 1,
            guest: "192.0.2.2".parse().unwrap(),
            gateway: "192.0.2.1".parse().unwrap(),
            mac: "02:00:00:00:00:02".into(),
        };
        for lose_proxy in [false, true] {
            let (packet, peer) = UnixDatagram::pair().unwrap();
            let (private, mut proxy) = UnixStream::pair().unwrap();
            let (extra, mut extra_peer) = UnixStream::pair().unwrap();
            // SAFETY: this fixture deliberately makes an unrelated FD inheritable.
            assert_eq!(
                unsafe { libc::fcntl(extra.as_raw_fd(), libc::F_SETFD, 0) },
                0
            );
            let mut helper = Helper::spawn(&path, packet, private, &config)
                .await
                .unwrap();
            let pid = helper.child.as_ref().unwrap().pid;
            drop(extra);
            assert_eq!(
                tokio::time::timeout(DEADLINE, extra_peer.read(&mut [0]))
                    .await
                    .unwrap()
                    .unwrap(),
                0,
                "helper inherited unrelated host descriptor"
            );
            helper.check_closed().unwrap();
            peer.send(&[0; 60]).unwrap();
            if lose_proxy {
                drop(proxy);
                tokio::time::timeout(DEADLINE, async {
                    while helper.check_closed().is_ok() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
                helper.close().await.unwrap();
                drop(helper);
            } else {
                helper.close().await.unwrap();
                assert_eq!(
                    tokio::time::timeout(DEADLINE, proxy.read(&mut [0]))
                        .await
                        .unwrap()
                        .unwrap(),
                    0
                );
                drop(helper);
            }
            // SAFETY: the helper owner must already have reaped its own child.
            assert_eq!(
                unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
            peer.set_nonblocking(true).unwrap();
            let result = peer.recv(&mut [0; 1600]);
            assert!(
                matches!(&result, Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::ConnectionReset)),
                "unexpected packet result: {result:?}"
            );
        }
    }

    #[tokio::test]
    async fn transport_readiness_must_be_exact_closed_epoch_zero() {
        for (version, sequence, epoch, state) in [
            (1, 0, 0, "ready"),
            (1, 0, 1, "closed"),
            (1, 1, 0, "closed"),
            (2, 0, 0, "closed"),
        ] {
            let (mut a, mut b) = UnixStream::pair().unwrap();
            private::write(
                &mut b,
                &Reply {
                    version,
                    sequence,
                    epoch,
                    state: state.into(),
                },
            )
            .await
            .unwrap();
            assert!(
                read_closed(&mut a).await.is_err(),
                "invalid CLOSED handshake accepted"
            );
        }
    }
}
