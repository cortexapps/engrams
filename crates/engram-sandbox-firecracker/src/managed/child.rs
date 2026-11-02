//! Process cleanup runs on a reserved native thread, including retained files.
use std::{
    io,
    process::{Child, Command},
    sync::{mpsc, Arc, Mutex},
};
struct State {
    child: Option<Child>,
    closed: bool,
}
pub(super) struct ChildOwner {
    state: Arc<Mutex<State>>,
    stop: mpsc::Sender<()>,
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
        let (stop, rx) = mpsc::channel::<()>();
        let (tx, done) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("managed-fc-cleanup".into())
            .spawn(move || {
                let _ = rx.recv();
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
    pub fn spawn(&mut self, command: &mut Command) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("child owner failed"))?;
        if state.closed || state.child.is_some() {
            return Err(io::Error::other("child owner already used"));
        }
        state.child = Some(command.spawn()?);
        Ok(())
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
    pub fn stop_signal(&self) -> mpsc::Sender<()> {
        self.stop.clone()
    }
    pub async fn close(&mut self) -> io::Result<()> {
        let _ = self.stop.send(());
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
        let _ = self.stop.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        owner
            .spawn(
                Command::new("/bin/cat")
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null()),
            )
            .unwrap();
        owner.live_pid().unwrap();
        let stop = owner.stop_signal();
        stop.send(()).unwrap();
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
        assert!(owner.spawn(&mut Command::new("/bin/cat")).is_err());
    }
}
