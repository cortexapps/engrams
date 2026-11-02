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
    child: Child,
    cleanup_failed: bool,
}
impl Helper {
    pub async fn spawn(
        path: &Path,
        packet: UnixDatagram,
        proxy: UnixStream,
        config: &Config,
    ) -> io::Result<Self> {
        let (mut control, child_control) = UnixStream::pair()?;
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
        private::write(&mut control, config).await?;
        let mut helper = Self {
            control,
            child,
            cleanup_failed: false,
        };
        read_closed(&mut helper.control).await?;
        helper.check_closed()?;
        Ok(helper)
    }
    /// Reliable-channel and process checks. Datagram EOF is never evidence.
    pub fn check_closed(&mut self) -> io::Result<()> {
        if self.cleanup_failed || self.child.poll()?.is_some() {
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
    pub fn close(&mut self) -> io::Result<()> {
        // SAFETY: control owns this live socket. Shutdown changes no FD ownership.
        unsafe {
            libc::shutdown(self.control.as_raw_fd(), libc::SHUT_RDWR);
        }
        if self.cleanup_failed {
            return Err(io::Error::other("network helper cleanup remains failed"));
        }
        let result = self.child.reap_bounded();
        self.cleanup_failed = result.is_err();
        result
    }
}
impl Drop for Helper {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            tracing::error!(%error, "network helper cleanup failed");
        }
    }
}

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
    #[tokio::test]
    async fn forced_cleanup_failure_is_terminal_and_child_is_reaped() {
        let (input, _keep_input_open) = std::os::unix::net::UnixStream::pair().unwrap();
        let (control, output) = UnixStream::pair().unwrap();
        let (errors, _read_errors) = UnixStream::pair().unwrap();
        // cat blocks on input and does not implement the reliable close barrier.
        let child = Child::spawn(
            Path::new("/bin/cat"),
            [input.as_raw_fd(), output.as_raw_fd(), errors.as_raw_fd()],
        )
        .unwrap();
        let pid = child.pid;
        drop((input, output, errors));
        let mut helper = Helper {
            control,
            child,
            cleanup_failed: false,
        };
        assert!(
            helper.close().is_err(),
            "unresponsive helper must fail cleanup"
        );
        assert!(
            helper.close().is_err(),
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
            let pid = helper.child.pid;
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
                drop(helper);
            } else {
                helper.close().unwrap();
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
