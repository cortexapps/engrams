use super::{child::ChildOwner, files::PinnedFiles, network::Namespace};
use std::{
    io,
    sync::{mpsc, Arc},
};

pub(super) struct Process {
    child: ChildOwner,
    namespace: Arc<Namespace>,
    path: std::path::PathBuf,
}
impl Process {
    pub fn reserve(namespace: Arc<Namespace>, files: Arc<PinnedFiles>) -> io::Result<Self> {
        let path = files.path.clone();
        let child = ChildOwner::reserve((namespace.clone(), files))?;
        Ok(Self {
            child,
            namespace,
            path,
        })
    }
    pub fn spawn(&mut self, binary: &std::path::Path) -> io::Result<()> {
        use std::process::Stdio;
        let log = std::fs::File::create_new(self.path.join("firecracker.log"))?;
        self.child.spawn(
            self.namespace
                .command(binary.as_os_str())?
                .arg("--api-sock")
                .arg(self.path.join("firecracker.sock"))
                .stdin(Stdio::null())
                .stdout(log.try_clone()?)
                .stderr(log),
        )
    }
    pub fn live_pid(&self) -> io::Result<u32> {
        self.child.live_pid()
    }
    pub fn stop_signal(&self) -> mpsc::Sender<()> {
        self.child.stop_signal()
    }
    pub async fn close(&mut self) -> io::Result<()> {
        self.child.close().await
    }
}
