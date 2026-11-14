//! Process cleanup runs on a reserved native thread, including retained files.
use std::{
    io,
    process::{Child, Command},
    sync::{mpsc, Arc, Mutex},
};
pub(super) enum Control {
    Spawn(Box<Command>, mpsc::SyncSender<io::Result<()>>),
    Stop,
}

struct State {
    child: Option<Child>,
    closed: bool,
}
pub(super) struct ChildOwner {
    state: Arc<Mutex<State>>,
    stop: mpsc::Sender<Control>,
    done: tokio::sync::oneshot::Receiver<io::Result<()>>,
    result: Option<Result<(), String>>,
}
impl ChildOwner {
    pub fn reserve(retained: impl Send + 'static) -> io::Result<Self> {
        let state = Arc::new(Mutex::new(State {
            child: None,
            closed: false,
        }));
        let worker = state.clone();
        let (stop, rx) = mpsc::channel::<Control>();
        let (tx, done) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("managed-fc-cleanup".into())
            .spawn(move || {
                while let Ok(Control::Spawn(mut command, reply)) = rx.recv() {
                    let mut state = worker.lock().expect("child lock");
                    let result = if state.closed || state.child.is_some() {
                        Err(io::Error::other("child owner already used"))
                    } else {
                        // This reserved thread stays alive through exact-child wait.
                        // Linux ties PDEATHSIG to the spawning thread, not its process.
                        #[cfg(target_os = "linux")]
                        unsafe {
                            use std::os::unix::process::CommandExt;
                            let parent = libc::getpid();
                            // Append to the namespace hook. Only syscalls and errno
                            // reads are allowed between fork and exec.
                            command.pre_exec(move || {
                                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                                    return Err(io::Error::last_os_error());
                                }
                                if libc::getppid() != parent {
                                    return Err(io::Error::from_raw_os_error(libc::ECHILD));
                                }
                                Ok(())
                            });
                        }
                        command.spawn().map(|child| state.child = Some(child))
                    };
                    let _ = reply.send(result);
                }
                // Reject queued spawns before potentially slow retained-file cleanup.
                drop(rx);
                let mut child = {
                    let mut state = worker.lock().expect("child lock");
                    state.closed = true;
                    state.child.take()
                };
                let result = if let Some(child) = &mut child {
                    match child.try_wait() {
                        Ok(Some(_)) => Ok(()),
                        Ok(None) => {
                            let kill = child.kill();
                            if kill
                                .as_ref()
                                .is_err_and(|e| e.raw_os_error() != Some(libc::ESRCH))
                            {
                                kill
                            } else {
                                child.wait().map(|_| ())
                            }
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    Ok(())
                };
                if result.is_err() {
                    // A failed cleanup cannot release files/namespace or authorize
                    // reuse. Preserve the ambiguous owner and report failure.
                    std::mem::forget((child, retained));
                    tracing::error!("managed VM cleanup failed");
                } else {
                    drop(child);
                    drop(retained);
                }
                let _ = tx.send(result);
            })?;
        Ok(Self {
            state,
            stop,
            done,
            result: None,
        })
    }
    pub fn spawn(&mut self, command: Command) -> io::Result<()> {
        let (reply, result) = mpsc::sync_channel(1);
        self.stop
            .send(Control::Spawn(Box::new(command), reply))
            .map_err(|_| io::Error::other("child owner closed"))?;
        result
            .recv()
            .map_err(|_| io::Error::other("child owner failed"))?
    }
    pub fn live_pid(&self) -> io::Result<u32> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("child owner failed"))?;
        let child = state
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("VM closed"))?;
        if child.try_wait()?.is_some() {
            return Err(io::Error::other("VM exited"));
        }
        // The sole Child owner is the only reaper. An exit after this read
        // leaves a zombie, whose PID cannot be reused by another process.
        Ok(child.id())
    }
    pub fn stop_signal(&self) -> mpsc::Sender<Control> {
        self.stop.clone()
    }
    pub async fn close(&mut self) -> io::Result<()> {
        let _ = self.stop.send(Control::Stop);
        if let Some(result) = &self.result {
            return result.clone().map_err(io::Error::other);
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), &mut self.done)
            .await
            .map_err(|_| io::Error::other("VM cleanup timed out"))
            .and_then(|r| r.map_err(|_| io::Error::other("VM cleanup owner lost")))
            .and_then(|r| r);
        self.result = Some(result.as_ref().map(|_| ()).map_err(ToString::to_string));
        result
    }
}
impl Drop for ChildOwner {
    fn drop(&mut self) {
        let _ = self.stop.send(Control::Stop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Uses the real owner in a separate process so SIGKILL cannot run Drop.
    #[cfg(target_os = "linux")]
    #[test]
    fn abrupt_parent_death_stops_exact_child_after_spawning_thread_exits() {
        use std::{
            os::fd::{AsRawFd, FromRawFd, OwnedFd},
            time::{Duration, Instant},
        };
        const HELPER: &str = "ENGRAM_CHILD_OWNER_TEST";
        if let Some(path) = std::env::var_os("ENGRAM_CHILD_OWNER_LEAF") {
            use std::io::Read;
            std::io::stdin().read_exact(&mut [0]).unwrap();
            std::fs::write(path, b"ready").unwrap();
            loop {
                std::thread::park();
            }
        }
        if let Some(path) = std::env::var_os(HELPER) {
            let leaf = std::path::PathBuf::from(&path).with_extension("leaf");
            let proof = leaf.clone();
            let owner = std::thread::spawn(move || {
                let mut owner = ChildOwner::reserve(()).unwrap();
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", "managed::child::tests::abrupt_parent_death_stops_exact_child_after_spawning_thread_exits"])
                    .env("ENGRAM_CHILD_OWNER_LEAF", proof)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null());
                owner.spawn(command).unwrap();
                owner
            }).join().unwrap();
            // A parent-death signal tied to the caller's short-lived thread
            // would stop the child here while the actual owner is still live.
            use std::io::Write;
            owner
                .state
                .lock()
                .unwrap()
                .child
                .as_mut()
                .unwrap()
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"?")
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !leaf.exists() {
                owner.live_pid().unwrap();
                assert!(
                    Instant::now() < deadline,
                    "child did not answer after caller thread exit"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            std::fs::write(path, owner.live_pid().unwrap().to_string()).unwrap();
            loop {
                std::thread::park();
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("child");
        let mut parent = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "managed::child::tests::abrupt_parent_death_stops_exact_child_after_spawning_thread_exits", "--nocapture"])
            .env(HELPER, &path).spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(
                parent.try_wait().unwrap().is_none(),
                "parent exited before child readiness"
            );
            if Instant::now() >= deadline {
                parent.kill().unwrap();
                parent.wait().unwrap();
                panic!("child readiness timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid: i32 = std::fs::read_to_string(path).unwrap().parse().unwrap();
        // The parent owns the unreaped child while we acquire its exact handle.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        assert!(fd >= 0, "pidfd_open: {}", io::Error::last_os_error());
        let child = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        let mut event = libc::pollfd {
            fd: child.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut event, 1, 0) },
            0,
            "child must survive caller thread exit"
        );
        parent.kill().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(parent.wait().unwrap().signal(), Some(libc::SIGKILL));
        let exited = unsafe { libc::poll(&mut event, 1, 2000) };
        // Clean the negative control through the exact handle, never a PID scan.
        if exited != 1 {
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    child.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                );
            }
        }
        assert_eq!(exited, 1, "child survived abrupt owner death");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retained_cleanup_does_not_block_unrelated_private_stop_or_cancel_ownership() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        struct Held {
            wait: mpsc::Receiver<()>,
            entered: mpsc::Sender<()>,
        }
        impl Drop for Held {
            fn drop(&mut self) {
                let _ = self.entered.send(());
                let _ = self.wait.recv();
            }
        }
        let (release, wait) = mpsc::channel();
        let (entered, entry) = mpsc::channel();
        let mut owner = ChildOwner::reserve(Held { wait, entered }).unwrap();
        let mut command = Command::new("/bin/cat");
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null());
        owner.spawn(command).unwrap();
        owner.live_pid().unwrap();
        let stop = owner.stop_signal();
        stop.send(Control::Stop).unwrap();
        tokio::time::timeout(std::time::Duration::from_millis(200), async {
            loop {
                if entry.try_recv().is_ok() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(owner.live_pid().is_err());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), owner.close())
                .await
                .is_err()
        );
        let (private, mut peer) = tokio::net::UnixStream::pair().unwrap();
        peer.shutdown().await.unwrap();
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            super::super::wait_for_stop(&private),
        )
        .await
        .unwrap();
        drop(private);
        assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
        release.send(()).unwrap();
        owner.close().await.unwrap();
        owner.close().await.unwrap();
        assert!(owner.spawn(Command::new("/bin/cat")).is_err());
    }
}
