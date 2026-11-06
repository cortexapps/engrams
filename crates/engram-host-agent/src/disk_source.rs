//! Lazy disk sources as processes of their own (ADR 0127 Decision 4).
//!
//! Each source is an `engram-disk-source serve` process. It outlives the
//! host-agent: it is not killed when the host-agent exits, it runs in the
//! node cgroup leaf `<parent>/<sandbox>-disk-<role>`, and its output goes
//! to `<work_dir>/disk-sources/<sandbox>-<role>.log`. A durable record per
//! role, `<work_dir>/disk-sources/<sandbox>-<role>.json`, names the
//! process, its device and the serve arguments. A source whose process
//! died is started again on the same device with `--recover`; the kernel
//! holds the device's I/O until then.
//!
//! A ublk device stopped or deleted while it is LIVE, with a request that
//! no server will finish, blocks in the kernel until the node reboots. So
//! a SIGTERM to a serving process deletes a device (the server fails the
//! reads it cannot serve, then deletes the device), a recovery that fails
//! ends with SIGKILL, which keeps the device for the next recovery, and a
//! stop recovers a dead source first. Only a device that no process
//! serves is deleted from outside, by `engram-disk-source delete`, which
//! refuses a LIVE device.
//!
//! Each start, recovery and stop runs in a task of its own, so a caller
//! that is dropped cannot leave a process without a record.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use engram_core::types::ids::SandboxId;
use engram_core::types::manifest::ManifestRef;
use engram_core::types::snapshot::DiskRole;
use engram_host_core::disk::Volume;
use engram_host_core::disk_sources::{DiskSources, Served};
use engram_host_core::{HostFs, TokioFs};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::ChildStdout;

use crate::durable_record;

const WHAT: &str = "disk source record";
const POLL: Duration = Duration::from_millis(20);
/// How many sources the startup recovery starts at once.
const RECOVER_AT_ONCE: usize = 4;

/// How [`ProcessSources`] runs its processes.
#[derive(Clone, Debug)]
pub struct ProcessSourcesConfig {
    /// The `engram-disk-source` binary.
    pub bin: PathBuf,
    /// Records and logs go in `<work_dir>/disk-sources`.
    pub work_dir: PathBuf,
    /// The node cgroup that holds each source's leaf. `None` keeps the
    /// sources in the host-agent's cgroup.
    pub cgroup_parent: Option<PathBuf>,
    /// The host chunk cache a source reads resident chunks from.
    pub cache_root: Option<PathBuf>,
    pub queues: u16,
    pub depth: u16,
    /// How long a process may take to report its device.
    pub ready_timeout: Duration,
    /// How long a process may take to exit after a signal, and a delete
    /// may take.
    pub stop_timeout: Duration,
}

impl ProcessSourcesConfig {
    pub fn new(bin: impl Into<PathBuf>, work_dir: impl Into<PathBuf>) -> Self {
        Self {
            bin: bin.into(),
            work_dir: work_dir.into(),
            cgroup_parent: None,
            cache_root: None,
            queues: 2,
            depth: 64,
            ready_timeout: Duration::from_secs(60),
            stop_timeout: Duration::from_secs(10),
        }
    }
}

/// One source process, as its durable record holds it. Two builds read
/// it during a roll: a field added later is `#[serde(default)]`, and an
/// older build ignores a field it does not know.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceRecord {
    pub sandbox: SandboxId,
    pub role: DiskRole,
    pub pid: u32,
    /// The process start time in clock ticks after boot (field 22 of
    /// `/proc/<pid>/stat`). With the pid it identifies the process, so a
    /// reused pid is never taken for the source.
    pub start_time: u64,
    /// `None` until the process reports its device.
    #[serde(default)]
    pub device: Option<Device>,
    /// A recovery must give the device its own queue count and depth.
    pub queues: u16,
    pub depth: u16,
    pub args: ServeArgs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: u32,
    pub path: PathBuf,
}

/// The arguments of `engram-disk-source serve`; a recovery repeats them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServeArgs {
    pub manifest: ManifestRef,
    #[serde(default)]
    pub base: Option<Base>,
    #[serde(default)]
    pub cache_root: Option<PathBuf>,
}

/// A staged volume and the manifest it holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Base {
    pub volume: PathBuf,
    pub manifest: ManifestRef,
}

/// What the host-agent reads about processes and ublk devices.
pub trait Probe: Send + Sync {
    /// The start time of `pid` while it runs. `None` when no such process
    /// runs; a zombie has exited.
    fn start_time(&self, pid: u32) -> Option<u64>;
    /// Whether the kernel has ublk device `id`, served or not.
    fn device_exists(&self, id: u32) -> bool;
    /// Whether the ublk driver is loaded.
    fn driver_loaded(&self) -> bool;
    /// Every ublk device id the kernel has.
    fn devices(&self) -> Vec<u32>;
    /// Every running `engram-disk-source` process, with its start time.
    fn source_processes(&self) -> Vec<(u32, u64)>;
}

/// [`Probe`] over `/proc` and sysfs.
pub struct KernelProbe;

/// The control nodes of the ublk devices; one lives from a device's add
/// to its delete.
const UBLK_CHAR_CLASS: &str = "/sys/class/ublk-char";

impl Probe for KernelProbe {
    fn start_time(&self, pid: u32) -> Option<u64> {
        use engram_sandbox_firecracker::sandbox_manifest::{
            is_zombie, read_proc_start_time_jiffies,
        };
        if is_zombie(pid) {
            return None;
        }
        read_proc_start_time_jiffies(pid)
    }

    fn device_exists(&self, id: u32) -> bool {
        Path::new(UBLK_CHAR_CLASS)
            .join(format!("ublkc{id}"))
            .exists()
    }

    fn driver_loaded(&self) -> bool {
        Path::new("/dev/ublk-control").exists()
    }

    fn devices(&self) -> Vec<u32> {
        let Ok(entries) = std::fs::read_dir(UBLK_CHAR_CLASS) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.strip_prefix("ublkc")?.parse().ok())
            .collect()
    }

    fn source_processes(&self) -> Vec<(u32, u64)> {
        use engram_sandbox_firecracker::sandbox_manifest::read_proc_comm;
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .filter(|&pid| {
                // The kernel keeps the first 15 bytes of the name.
                read_proc_comm(pid).is_some_and(|comm| comm == "engram-disk-source"[..15])
            })
            .filter_map(|pid| Some((pid, self.start_time(pid)?)))
            .collect()
    }
}

/// The state of a recorded source.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Health {
    /// The recorded process runs and has reported its device.
    Serving(Device),
    /// The process died. The kernel holds the device for a recovery.
    Dead(Device),
    /// A start that did not report its device; its process still runs.
    Unreported,
    /// No device is left to serve: the process died before it reported
    /// one (`None`), or a stop deleted the device and did not finish.
    Gone(Option<u32>),
}

/// `device_lost`: the kernel has no device of that id, or another record
/// names it (a device made after this one's was deleted).
fn health(device: Option<Device>, alive: bool, device_lost: bool) -> Health {
    match (device, alive) {
        (Some(device), true) => Health::Serving(device),
        (None, true) => Health::Unreported,
        (Some(device), false) if !device_lost => Health::Dead(device),
        (device, false) => Health::Gone(device.map(|d| d.id)),
    }
}

/// Run `op` in a task of its own, so a caller that is dropped cannot stop
/// it between a spawn and the record of the process.
async fn detached<T: Send + 'static>(
    op: impl Future<Output = io::Result<T>> + Send + 'static,
) -> io::Result<T> {
    tokio::spawn(op).await.map_err(io::Error::other)?
}

/// [`DiskSources`] as `engram-disk-source` processes, one per role.
pub struct ProcessSources(Arc<Sources>);

/// The state and steps of [`ProcessSources`], shared with the tasks that
/// run each step.
struct Sources {
    cfg: ProcessSourcesConfig,
    dir: PathBuf,
    fs: Arc<dyn HostFs>,
    probe: Arc<dyn Probe>,
    /// One lock per role: a start, a recovery and a stop of one role never
    /// overlap.
    locks: std::sync::Mutex<HashMap<RoleKey, Arc<tokio::sync::Mutex<()>>>>,
}

type RoleKey = (SandboxId, DiskRole);

impl ProcessSources {
    pub fn new(cfg: ProcessSourcesConfig) -> Self {
        Self::with_probe(cfg, Arc::new(TokioFs), Arc::new(KernelProbe))
    }

    pub fn with_probe(
        cfg: ProcessSourcesConfig,
        fs: Arc<dyn HostFs>,
        probe: Arc<dyn Probe>,
    ) -> Self {
        let dir = cfg.work_dir.join("disk-sources");
        Self(Arc::new(Sources {
            cfg,
            dir,
            fs,
            probe,
            locks: Default::default(),
        }))
    }

    /// Whether this host can start a source; the reason when it cannot.
    pub fn check_driver(&self) -> io::Result<()> {
        self.0.check_driver()
    }

    /// Every record on this host.
    pub async fn records(&self) -> Vec<SourceRecord> {
        self.0.records().await
    }

    /// Log every source process and ublk device that no record names, and
    /// delete each such device that no process serves. Run once at
    /// startup, before anything starts a source. A device a running
    /// process serves is never deleted. A device whose server died is
    /// deleted only when every record file could be read: no drive is
    /// made over a device before its record names it, so such a device
    /// has no reader.
    pub async fn sweep_unrecorded(&self) {
        let sources = self.0.clone();
        if let Err(error) = detached(async move { sources.sweep_unrecorded().await }).await {
            tracing::warn!(%error, "disk source sweep failed");
        }
    }
}

impl Sources {
    fn check_driver(&self) -> io::Result<()> {
        if self.probe.driver_loaded() {
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the ublk driver is not loaded (/dev/ublk-control is absent), so this host cannot \
             start a disk source; node prep loads ublk_drv",
        ))
    }

    async fn lock(&self, key: RoleKey) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock().unwrap();
            // An entry only the map holds has no holder and no waiter.
            locks.retain(|k, l| *k == key || Arc::strong_count(l) > 1);
            locks.entry(key).or_default().clone()
        };
        lock.lock_owned().await
    }

    fn id(sandbox: SandboxId, role: DiskRole) -> String {
        format!("{sandbox}-{role}")
    }

    fn log_path(&self, sandbox: SandboxId, role: DiskRole) -> PathBuf {
        self.dir.join(format!("{}.log", Self::id(sandbox, role)))
    }

    fn leaf(sandbox: SandboxId, role: DiskRole) -> String {
        format!("{sandbox}-disk-{role}")
    }

    async fn load(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<Option<SourceRecord>> {
        durable_record::load(self.fs.as_ref(), &self.dir, Self::id(sandbox, role), WHAT).await
    }

    async fn persist(&self, record: &SourceRecord) -> io::Result<()> {
        let id = Self::id(record.sandbox, record.role);
        durable_record::persist(self.fs.as_ref(), &self.dir, id, record, WHAT).await
    }

    async fn records(&self) -> Vec<SourceRecord> {
        durable_record::load_all(self.fs.as_ref(), &self.dir, WHAT).await
    }

    /// How many record files there are, readable or not.
    async fn record_files(&self) -> usize {
        match self.fs.read_dir(&self.dir).await {
            Ok(paths) => paths
                .iter()
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
                .count(),
            Err(_) => 0,
        }
    }

    async fn health(&self, record: &SourceRecord) -> Health {
        let alive = self.probe.start_time(record.pid) == Some(record.start_time);
        let lost = match (&record.device, alive) {
            (Some(device), false) => {
                !self.probe.device_exists(device.id)
                    || self.records().await.iter().any(|other| {
                        (other.sandbox, other.role) != (record.sandbox, record.role)
                            && other.device.as_ref().map(|d| d.id) == Some(device.id)
                    })
            }
            _ => false,
        };
        health(record.device.clone(), alive, lost)
    }

    /// Spawn `serve` with `queues` and `depth`, on device `recover` when it
    /// is set, and move it to the role's cgroup leaf. Returns the pid, its
    /// start time and the stdout that carries the ready line.
    async fn spawn(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        args: &ServeArgs,
        (queues, depth): (u16, u16),
        recover: Option<u32>,
    ) -> io::Result<(u32, u64, ChildStdout)> {
        self.fs.create_dir(&self.dir).await?;
        let log_path = self.log_path(sandbox, role);
        // A file, never a pipe: a pipe breaks when the host-agent exits.
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let mut cmd = tokio::process::Command::new(&self.cfg.bin);
        cmd.arg("serve")
            .arg("--manifest")
            .arg(args.manifest.to_string())
            .arg("--queues")
            .arg(queues.to_string())
            .arg("--depth")
            .arg(depth.to_string());
        if let Some(base) = &args.base {
            cmd.arg("--base-volume")
                .arg(&base.volume)
                .arg("--base-manifest")
                .arg(base.manifest.to_string());
        }
        if let Some(root) = &args.cache_root {
            cmd.arg("--cache-root").arg(root);
        }
        if let Some(id) = recover {
            cmd.arg("--recover").arg(id.to_string());
        }
        // No kill_on_drop: the source outlives the host-agent.
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log))
            .spawn()
            .map_err(|e| {
                io::Error::new(e.kind(), format!("spawn {}: {e}", self.cfg.bin.display()))
            })?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let pid = child
            .id()
            .expect("a child that was not waited for has a pid");
        let start_time = self.probe.start_time(pid);
        // Reaps the process when it exits while this host-agent runs.
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        let Some(start_time) = start_time else {
            return Err(io::Error::other(format!(
                "disk source {} exited at once; see {}",
                Self::id(sandbox, role),
                log_path.display()
            )));
        };
        if let Some(parent) = &self.cfg.cgroup_parent {
            let leaf = Self::leaf(sandbox, role);
            if let Err(e) = engram_sandbox_firecracker::escape_to_node_cgroup(parent, &leaf, &[pid])
            {
                tracing::warn!(
                    %sandbox, %role, pid, parent = %parent.display(), error = %e,
                    "could not move the disk source to its node cgroup; a host-agent pod \
                     restart kills it, and the next host-agent recovers it",
                );
            }
        }
        Ok((pid, start_time, stdout))
    }

    /// Wait for the ready line `<device path> <device id>`.
    async fn ready(
        &self,
        stdout: ChildStdout,
        sandbox: SandboxId,
        role: DiskRole,
    ) -> io::Result<Device> {
        let id = Self::id(sandbox, role);
        let log = self.log_path(sandbox, role);
        let mut line = String::new();
        let mut stdout = BufReader::new(stdout);
        let read = stdout.read_line(&mut line);
        match tokio::time::timeout(self.cfg.ready_timeout, read).await {
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "disk source {id} did not serve within {:?}; see {}",
                    self.cfg.ready_timeout,
                    log.display()
                ),
            )),
            Ok(Err(e)) => Err(e),
            Ok(Ok(0)) => Err(io::Error::other(format!(
                "disk source {id} exited before it served; see {}",
                log.display()
            ))),
            Ok(Ok(_)) => parse_ready(&line),
        }
    }

    /// Delete device `id`, which no process serves, with
    /// `engram-disk-source delete`. `quiesced` also deletes a device whose
    /// server died; the binary refuses a device that a process serves.
    async fn delete_device(&self, id: u32, quiesced: bool) -> io::Result<()> {
        let mut cmd = tokio::process::Command::new(&self.cfg.bin);
        cmd.arg("delete").arg("--device").arg(id.to_string());
        if quiesced {
            cmd.arg("--quiesced");
        }
        cmd.stdin(Stdio::null()).kill_on_drop(true);
        let output = tokio::time::timeout(self.cfg.stop_timeout, cmd.output())
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("the delete of device {id} did not finish"),
                )
            })??;
        if output.status.success() {
            return Ok(());
        }
        Err(io::Error::other(format!(
            "delete device {id}: {} ({})",
            String::from_utf8_lossy(&output.stderr).trim(),
            output.status
        )))
    }

    /// Send `signal` to the process if it is still the recorded one.
    fn signal(&self, pid: u32, start_time: u64, signal: nix::sys::signal::Signal) {
        if self.probe.start_time(pid) != Some(start_time) {
            return;
        }
        let Ok(raw) = i32::try_from(pid) else { return };
        if let Err(e) = nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), signal) {
            tracing::debug!(pid, %signal, error = %e, "signal the disk source");
        }
    }

    /// Whether the process exits within the stop timeout.
    async fn exits(&self, pid: u32, start_time: u64) -> bool {
        let polls = self.cfg.stop_timeout.as_millis() / POLL.as_millis() + 1;
        for _ in 0..polls {
            if self.probe.start_time(pid) != Some(start_time) {
                return true;
            }
            tokio::time::sleep(POLL).await;
        }
        self.probe.start_time(pid) != Some(start_time)
    }

    /// End a process: SIGTERM, then SIGKILL if it does not exit in time.
    /// SIGTERM deletes the device of a process that serves, so this is for
    /// a source that nothing reads. Whether the process exited.
    async fn terminate(&self, pid: u32, start_time: u64) -> bool {
        use nix::sys::signal::Signal;
        self.signal(pid, start_time, Signal::SIGTERM);
        if self.exits(pid, start_time).await {
            return true;
        }
        tracing::warn!(pid, timeout = ?self.cfg.stop_timeout, "disk source ignored SIGTERM; SIGKILL");
        self.kill(pid, start_time).await
    }

    /// SIGKILL: the device stays for a recovery. Whether the process exited.
    async fn kill(&self, pid: u32, start_time: u64) -> bool {
        self.signal(pid, start_time, nix::sys::signal::Signal::SIGKILL);
        self.exits(pid, start_time).await
    }

    /// Remove what a role's source leaves on the host; the record last.
    async fn clean_up(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<()> {
        engram_sandbox_firecracker::remove_node_cgroup(
            self.cfg.cgroup_parent.as_deref(),
            &Self::leaf(sandbox, role),
        );
        match self.fs.remove_file(&self.log_path(sandbox, role)).await {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        durable_record::delete(self.fs.as_ref(), &self.dir, Self::id(sandbox, role)).await
    }

    /// Start a recovery process on the record's device, with the device's
    /// own queues and depth.
    async fn recover(&self, record: &SourceRecord, device: &Device) -> io::Result<SourceRecord> {
        self.check_driver()?;
        let (sandbox, role) = (record.sandbox, record.role);
        let shape = (record.queues, record.depth);
        let (pid, start_time, stdout) = self
            .spawn(sandbox, role, &record.args, shape, Some(device.id))
            .await?;
        let next = SourceRecord {
            pid,
            start_time,
            ..record.clone()
        };
        // Before the ready line: a host-agent that dies now leaves a record
        // that names the process that serves the device.
        if let Err(e) = self.persist(&next).await {
            self.kill(pid, start_time).await;
            return Err(e);
        }
        let served = self.ready(stdout, sandbox, role).await;
        match served {
            Ok(served) if served == *device => {
                tracing::info!(%sandbox, %role, pid, device = device.id, "disk source recovered");
                Ok(next)
            }
            other => {
                self.kill(pid, start_time).await;
                Err(match other {
                    Ok(served) => io::Error::other(format!(
                        "the recovery of device {} serves {served:?}",
                        device.id
                    )),
                    Err(e) => e,
                })
            }
        }
    }

    /// Make `record`'s source serve, and return its record and device.
    /// `NotFound`, after its residue is removed, when no device is left.
    async fn revive(&self, record: SourceRecord) -> io::Result<(SourceRecord, Device)> {
        let (sandbox, role) = (record.sandbox, record.role);
        let id = Self::id(sandbox, role);
        let gone = match self.health(&record).await {
            Health::Serving(device) => return Ok((record, device)),
            Health::Dead(device) => {
                tracing::warn!(%sandbox, %role, pid = record.pid, device = device.id,
                    "disk source process is gone; recovering its device");
                return Ok((self.recover(&record, &device).await?, device));
            }
            Health::Unreported => {
                if !self.terminate(record.pid, record.start_time).await {
                    return Err(io::Error::other(format!(
                        "disk source {id} (pid {}) did not report a device and does not exit",
                        record.pid
                    )));
                }
                "it did not report a device; it is stopped".to_string()
            }
            Health::Gone(Some(device)) => format!("its device {device} is gone"),
            Health::Gone(None) => "it exited before it reported a device".to_string(),
        };
        tracing::warn!(%sandbox, %role, reason = %gone, "disk source record removed");
        self.clean_up(sandbox, role).await?;
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("disk source {id}: {gone}"),
        ))
    }

    async fn start(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        manifest: ManifestRef,
        base: Option<Base>,
    ) -> io::Result<PathBuf> {
        self.check_driver()?;
        let _guard = self.lock((sandbox, role)).await;
        if self.load(sandbox, role).await?.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("disk source {} already exists", Self::id(sandbox, role)),
            ));
        }
        let args = ServeArgs {
            manifest,
            base,
            cache_root: self.cfg.cache_root.clone(),
        };
        let shape = (self.cfg.queues, self.cfg.depth);
        let (pid, start_time, stdout) = self.spawn(sandbox, role, &args, shape, None).await?;
        let record = SourceRecord {
            sandbox,
            role,
            pid,
            start_time,
            device: None,
            queues: self.cfg.queues,
            depth: self.cfg.depth,
            args,
        };
        let served = async {
            // Before the ready line, so a host-agent that dies now leaves a
            // record of the process.
            self.persist(&record).await?;
            let device = self.ready(stdout, sandbox, role).await?;
            let serving = SourceRecord {
                device: Some(device.clone()),
                ..record
            };
            self.persist(&serving).await?;
            Ok::<_, io::Error>(device.path)
        };
        match served.await {
            Ok(path) => Ok(path),
            Err(e) => {
                // Nothing reads the device yet; SIGTERM deletes it.
                if self.terminate(pid, start_time).await {
                    self.clean_up(sandbox, role).await?;
                }
                Err(e)
            }
        }
    }

    async fn ensure_alive(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<()> {
        let _guard = self.lock((sandbox, role)).await;
        let record = self
            .load(sandbox, role)
            .await?
            .ok_or_else(|| no_source(sandbox, role))?;
        self.revive(record).await.map(drop)
    }

    async fn stop(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<()> {
        let _guard = self.lock((sandbox, role)).await;
        let Some(record) = self.load(sandbox, role).await? else {
            return Ok(());
        };
        let device = match self.revive(record).await {
            Ok((record, device)) => {
                if !self.terminate(record.pid, record.start_time).await {
                    return Err(io::Error::other(format!(
                        "disk source {} (pid {}) does not exit",
                        Self::id(sandbox, role),
                        record.pid
                    )));
                }
                device
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            // The kernel refuses a recovery of a device that its server
            // stopped and did not delete. With no process left, a delete
            // is the one way to remove it.
            Err(e) => match self.load(sandbox, role).await? {
                Some(SourceRecord {
                    pid,
                    start_time,
                    device: Some(device),
                    ..
                }) if self.probe.start_time(pid) != Some(start_time) => {
                    tracing::warn!(%sandbox, %role, device = device.id, error = %e,
                        "disk source cannot be recovered; deleting its device");
                    device
                }
                _ => return Err(e),
            },
        };
        if self.probe.device_exists(device.id) {
            // The caller removed every drive over the device, so nothing
            // reads it: a device whose server died may go too.
            if let Err(e) = self.delete_device(device.id, true).await {
                tracing::warn!(%sandbox, %role, device = device.id, error = %e,
                    "device of a stopped disk source not deleted");
            }
        }
        if self.probe.device_exists(device.id) {
            // The record stays, so the next stop tries again.
            return Err(io::Error::other(format!(
                "device {} of disk source {} stays after its process exited",
                device.id,
                Self::id(sandbox, role)
            )));
        }
        self.clean_up(sandbox, role).await
    }

    async fn sweep_unrecorded(&self) -> io::Result<()> {
        let records = self.records().await;
        if !self.probe.driver_loaded() {
            if !records.is_empty() {
                tracing::error!(
                    records = records.len(),
                    "disk source records exist, but the ublk driver is not loaded; their \
                     sources cannot serve"
                );
            }
            return Ok(());
        }
        let readable = self.record_files().await == records.len();
        let processes: HashSet<(u32, u64)> =
            records.iter().map(|r| (r.pid, r.start_time)).collect();
        for (pid, start_time) in self.probe.source_processes() {
            if !processes.contains(&(pid, start_time)) {
                tracing::warn!(pid, "a disk source process runs that no record names");
            }
        }
        let devices: HashSet<u32> = records
            .iter()
            .filter_map(|r| r.device.as_ref().map(|d| d.id))
            .collect();
        for id in self.probe.devices() {
            if devices.contains(&id) {
                continue;
            }
            match self.delete_device(id, readable).await {
                Ok(()) => tracing::warn!(device = id, "deleted a ublk device that no record names"),
                Err(error) => tracing::warn!(
                    device = id,
                    %error,
                    "a ublk device that no record names stays"
                ),
            }
        }
        Ok(())
    }
}

fn parse_ready(line: &str) -> io::Result<Device> {
    let parsed = line.trim().split_once(' ').and_then(|(path, id)| {
        Some(Device {
            id: id.parse().ok()?,
            path: PathBuf::from(path),
        })
    });
    parsed.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("disk source ready line {line:?} is not \"<device path> <device id>\""),
        )
    })
}

fn no_source(sandbox: SandboxId, role: DiskRole) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("the {role} disk of sandbox {sandbox} has no source"),
    )
}

#[async_trait]
impl DiskSources for ProcessSources {
    fn can_start(&self) -> bool {
        self.0.probe.driver_loaded()
    }

    async fn start(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        manifest: ManifestRef,
        base: Option<(&Volume, ManifestRef)>,
    ) -> io::Result<PathBuf> {
        let base = base.map(|(volume, manifest)| Base {
            volume: volume.path.clone(),
            manifest,
        });
        let sources = self.0.clone();
        detached(async move { sources.start(sandbox, role, manifest, base).await }).await
    }

    async fn ensure_alive(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<()> {
        let sources = self.0.clone();
        detached(async move { sources.ensure_alive(sandbox, role).await }).await
    }

    async fn served(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<Served> {
        let record = self
            .0
            .load(sandbox, role)
            .await?
            .ok_or_else(|| no_source(sandbox, role))?;
        let device = record.device.ok_or_else(|| no_source(sandbox, role))?;
        Ok(Served {
            device: device.path,
            manifest: record.args.manifest,
            base: record.args.base.map(|b| b.manifest),
        })
    }

    async fn stop(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<()> {
        let sources = self.0.clone();
        detached(async move { sources.stop(sandbox, role).await }).await
    }

    async fn list(&self) -> io::Result<Vec<(SandboxId, DiskRole)>> {
        Ok(self
            .0
            .records()
            .await
            .into_iter()
            .map(|r| (r.sandbox, r.role))
            .collect())
    }
}

/// Make every recorded source serve, after the pool opens and before the
/// volume reconcile, which removes drives: a drive over a dead source
/// cannot be removed. A few recoveries run at once, so sources that each
/// take the full ready timeout do not add up.
pub async fn recover_all(sources: &dyn DiskSources) {
    use futures::StreamExt;
    let roles = match sources.list().await {
        Ok(roles) => roles,
        Err(error) => {
            tracing::warn!(%error, "disk source recovery: cannot list the sources");
            return;
        }
    };
    futures::stream::iter(roles)
        .for_each_concurrent(RECOVER_AT_ONCE, |(sandbox, role)| async move {
            match sources.ensure_alive(sandbox, role).await {
                Ok(()) => tracing::debug!(%sandbox, %role, "disk source serves"),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    tracing::info!(%sandbox, %role, %error, "disk source removed");
                }
                Err(error) => {
                    tracing::error!(%sandbox, %role, %error, "disk source recovery failed");
                }
            }
        })
        .await;
}

#[cfg(test)]
mod tests {
    //! Records and the restart decision, with a fake probe and a shell
    //! script in place of the binary; the kernel side is the root lane's
    //! (`tests/disk_source_ublk.rs`). Crash states follow ADR 0099 H5: each
    //! post-crash file state is built directly.

    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    use super::*;

    /// Processes run unless the test says otherwise. A device exists while
    /// a file of its id is in `devices`, which the fake binary's `delete`
    /// removes.
    struct FakeProbe {
        start_times: Mutex<HashMap<u32, Option<u64>>>,
        devices: PathBuf,
        processes: Mutex<Vec<(u32, u64)>>,
        no_driver: bool,
    }

    impl Probe for FakeProbe {
        fn start_time(&self, pid: u32) -> Option<u64> {
            if let Some(time) = self.start_times.lock().unwrap().get(&pid) {
                return *time;
            }
            let pid = nix::unistd::Pid::from_raw(i32::try_from(pid).ok()?);
            nix::sys::signal::kill(pid, None).is_ok().then_some(1)
        }
        fn device_exists(&self, id: u32) -> bool {
            self.devices.join(id.to_string()).exists()
        }
        fn driver_loaded(&self) -> bool {
            !self.no_driver
        }
        fn devices(&self) -> Vec<u32> {
            std::fs::read_dir(&self.devices)
                .unwrap()
                .filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                .collect()
        }
        fn source_processes(&self) -> Vec<(u32, u64)> {
            self.processes.lock().unwrap().clone()
        }
    }

    struct Rig {
        dir: tempfile::TempDir,
        probe: Arc<FakeProbe>,
        sources: ProcessSources,
    }

    /// `script` is the body of the fake binary. `$ARGS` names a file that
    /// `serve` writes its arguments to, `$DELETES` one that `delete`
    /// appends its arguments to, and `$DEVS` the fake's device folder.
    fn rig(script: &str, no_driver: bool) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let devices = dir.path().join("devices");
        std::fs::create_dir(&devices).unwrap();
        let bin = dir.path().join("fake-disk-source");
        let body = script
            .replace("$ARGS", &dir.path().join("args").display().to_string())
            .replace(
                "$DELETES",
                &dir.path().join("deletes").display().to_string(),
            )
            .replace("$DEVS", &devices.display().to_string());
        std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut cfg = ProcessSourcesConfig::new(bin, dir.path());
        cfg.ready_timeout = Duration::from_secs(10);
        cfg.stop_timeout = Duration::from_secs(5);
        let probe = Arc::new(FakeProbe {
            start_times: Default::default(),
            devices,
            processes: Default::default(),
            no_driver,
        });
        let sources = ProcessSources::with_probe(cfg, Arc::new(TokioFs), probe.clone());
        Rig {
            dir,
            probe,
            sources,
        }
    }

    const DELETES: &str =
        "if [ \"$1\" = delete ]; then echo \"$@\" >> $DELETES; rm -f $DEVS/$3; exit 0; fi";
    const DELETE_REFUSED: &str =
        "if [ \"$1\" = delete ]; then echo \"$@\" >> $DELETES; echo refused >&2; exit 1; fi";
    const RECOVERY_REFUSED: &str = "case \"$*\" in *--recover*) exit 1;; esac";
    const SERVE: &str = "echo \"$@\" > $ARGS\necho /dev/ublkb7 7\nexec sleep 60";

    fn serves() -> String {
        format!("{DELETES}\n{SERVE}")
    }

    impl Rig {
        fn args(&self) -> Option<String> {
            std::fs::read_to_string(self.dir.path().join("args")).ok()
        }
        fn deletes(&self) -> Option<String> {
            std::fs::read_to_string(self.dir.path().join("deletes")).ok()
        }
        fn record_path(&self, sandbox: SandboxId, role: DiskRole) -> PathBuf {
            durable_record::record_path(&self.sources.0.dir, Sources::id(sandbox, role))
        }
        fn mark_dead(&self, pid: u32) {
            self.probe.start_times.lock().unwrap().insert(pid, None);
        }
        fn mark_alive(&self, pid: u32) {
            self.probe.start_times.lock().unwrap().insert(pid, Some(1));
        }
        fn add_device(&self, id: u32) {
            std::fs::write(self.probe.devices.join(id.to_string()), b"").unwrap();
        }
        async fn persist(&self, record: &SourceRecord) {
            self.sources.0.persist(record).await.unwrap();
        }
    }

    fn record(sandbox: SandboxId, role: DiskRole, pid: u32, device: Option<u32>) -> SourceRecord {
        SourceRecord {
            sandbox,
            role,
            pid,
            start_time: 1,
            device: device.map(|id| Device {
                id,
                path: PathBuf::from(format!("/dev/ublkb{id}")),
            }),
            queues: 3,
            depth: 9,
            args: ServeArgs {
                manifest: ManifestRef::new(),
                base: Some(Base {
                    volume: PathBuf::from("/dev/engram/base"),
                    manifest: ManifestRef::new(),
                }),
                cache_root: Some(PathBuf::from("/cache")),
            },
        }
    }

    fn kill_now(pid: u32) {
        let pid = nix::unistd::Pid::from_raw(pid as i32);
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
    }

    #[test]
    fn health_restarts_only_a_dead_process_whose_device_is_its_own() {
        let device = Device {
            id: 3,
            path: PathBuf::from("/dev/ublkb3"),
        };
        let some = || Some(device.clone());
        assert_eq!(health(some(), true, false), Health::Serving(device.clone()));
        assert_eq!(health(some(), false, false), Health::Dead(device.clone()));
        assert_eq!(health(some(), false, true), Health::Gone(Some(3)));
        assert_eq!(health(None, true, false), Health::Unreported);
        assert_eq!(health(None, false, false), Health::Gone(None));
    }

    /// The build before a field was added wrote no such field, and the
    /// build after it writes one this build does not know.
    #[test]
    fn a_record_reads_across_one_release_in_both_directions() {
        let rec = record(SandboxId::new(), DiskRole::Root, 9, None);
        let mut json = serde_json::to_value(&rec).unwrap();
        let fields = json.as_object_mut().unwrap();
        fields.remove("device");
        fields["args"].as_object_mut().unwrap().remove("cache_root");
        fields.insert("a_later_field".into(), serde_json::json!(1));
        let read: SourceRecord = serde_json::from_value(json).unwrap();
        let expected = SourceRecord {
            args: ServeArgs {
                cache_root: None,
                ..rec.args.clone()
            },
            ..rec
        };
        assert_eq!(read, expected);
    }

    #[tokio::test]
    async fn a_record_round_trips_and_names_what_the_source_serves() {
        let r = rig(&serves(), false);
        let (sandbox, role) = (SandboxId::new(), DiskRole::Swap);
        let rec = record(sandbox, role, 4242, Some(5));
        r.persist(&rec).await;

        assert_eq!(r.sources.records().await, vec![rec.clone()]);
        assert_eq!(r.sources.list().await.unwrap(), vec![(sandbox, role)]);
        assert_eq!(
            r.sources.served(sandbox, role).await.unwrap(),
            Served {
                device: PathBuf::from("/dev/ublkb5"),
                manifest: rec.args.manifest,
                base: rec.args.base.map(|b| b.manifest),
            }
        );
    }

    #[tokio::test]
    async fn a_torn_record_is_an_error_for_its_role_and_skipped_in_the_list() {
        let r = rig(&serves(), false);
        let (torn, sibling) = (SandboxId::new(), SandboxId::new());
        let role = DiskRole::Root;
        r.persist(&record(sibling, role, 1, Some(1))).await;
        r.persist(&record(torn, role, 2, Some(2))).await;
        let path = r.record_path(torn, role);
        let full = std::fs::read(&path).unwrap();
        for offset in 0..full.len() {
            std::fs::write(&path, &full[..offset]).unwrap();
            assert_eq!(
                r.sources.list().await.unwrap(),
                vec![(sibling, role)],
                "offset {offset}"
            );
            let err = r.sources.served(torn, role).await.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "offset {offset}");
        }
        // A record that cannot be read is never taken as no source: a stop
        // could otherwise leave a device without a server.
        for err in [
            r.sources.ensure_alive(torn, role).await.unwrap_err(),
            r.sources.stop(torn, role).await.unwrap_err(),
        ] {
            assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[tokio::test]
    async fn a_missing_record_is_no_source() {
        let r = rig(&serves(), false);
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        assert!(r.sources.list().await.unwrap().is_empty());
        for err in [
            r.sources.served(sandbox, role).await.unwrap_err(),
            r.sources.ensure_alive(sandbox, role).await.unwrap_err(),
        ] {
            assert_eq!(err.kind(), io::ErrorKind::NotFound);
        }
        r.sources.stop(sandbox, role).await.unwrap();
    }

    #[tokio::test]
    async fn garbage_siblings_are_not_records() {
        let r = rig(&serves(), false);
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        let rec = record(sandbox, role, 1, Some(1));
        r.persist(&rec).await;
        let dir = &r.sources.0.dir;
        // A crash before the rename, a log, and junk.
        let other = record(SandboxId::new(), role, 2, Some(2));
        let other_id = Sources::id(other.sandbox, role);
        let sealed =
            crate::durable_envelope::seal(&other_id, &serde_json::to_string(&other).unwrap());
        std::fs::write(dir.join(format!("{other_id}.json.partial")), sealed).unwrap();
        std::fs::write(r.sources.0.log_path(sandbox, role), b"x").unwrap();
        std::fs::write(dir.join("junk.json"), b"\x00\xff not json").unwrap();
        assert_eq!(r.sources.records().await, vec![rec]);
    }

    #[tokio::test]
    async fn a_live_source_is_left_alone() {
        let r = rig(&serves(), false);
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        let rec = record(sandbox, role, 4242, Some(5));
        r.mark_alive(4242);
        r.persist(&rec).await;
        r.sources.ensure_alive(sandbox, role).await.unwrap();
        assert_eq!(r.args(), None, "nothing was started");
        assert_eq!(r.sources.records().await, vec![rec]);
    }

    /// A dead process, or a live one that is not the recorded process
    /// (its pid was reused), is started again on the same device with the
    /// same arguments, queues and depth.
    #[tokio::test]
    async fn a_gone_or_reused_pid_is_recovered_on_the_same_device() {
        for reused in [false, true] {
            let r = rig(&serves(), false);
            let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
            let rec = record(sandbox, role, 4242, Some(7));
            let time = if reused { Some(99) } else { None };
            r.probe.start_times.lock().unwrap().insert(4242, time);
            r.add_device(7);
            r.persist(&rec).await;

            r.sources.ensure_alive(sandbox, role).await.unwrap();

            let base = rec.args.base.as_ref().unwrap();
            assert_eq!(
                r.args().unwrap().trim(),
                format!(
                    "serve --manifest {} --queues 3 --depth 9 --base-volume /dev/engram/base \
                     --base-manifest {} --cache-root /cache --recover 7",
                    rec.args.manifest, base.manifest
                ),
                "reused={reused}"
            );
            let now = r.sources.records().await.pop().unwrap();
            assert_ne!(now.pid, 4242);
            let before = SourceRecord {
                pid: 4242,
                start_time: 1,
                ..now.clone()
            };
            assert_eq!(before, rec, "only the process changes");
            kill_now(now.pid);
        }
    }

    #[tokio::test]
    async fn a_source_whose_device_is_gone_is_removed() {
        let r = rig(&serves(), false);
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        r.mark_dead(4242);
        r.persist(&record(sandbox, role, 4242, Some(7))).await;
        let err = r.sources.ensure_alive(sandbox, role).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(r.sources.records().await.is_empty());
        assert_eq!(r.args(), None);
    }

    /// The kernel reuses a deleted device's id. A dead source whose device
    /// id another record names lost its device to a stop that did not
    /// finish; a recovery would attach to the other source's device.
    #[tokio::test]
    async fn a_device_id_another_source_holds_is_not_recovered() {
        let r = rig(&serves(), false);
        let role = DiskRole::Root;
        let (stale, owner) = (SandboxId::new(), SandboxId::new());
        r.mark_dead(4242);
        r.mark_alive(4343);
        r.add_device(7);
        r.persist(&record(stale, role, 4242, Some(7))).await;
        let held = record(owner, role, 4343, Some(7));
        r.persist(&held).await;

        let err = r.sources.ensure_alive(stale, role).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(r.sources.records().await, vec![held]);
        assert_eq!(r.args(), None);
    }

    #[tokio::test]
    async fn a_start_that_never_reported_a_device_is_removed() {
        let r = rig(&serves(), false);
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        r.mark_dead(4242);
        r.persist(&record(sandbox, role, 4242, None)).await;
        let err = r.sources.ensure_alive(sandbox, role).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(r.sources.records().await.is_empty());
    }

    #[tokio::test]
    async fn start_records_the_device_and_stop_removes_every_trace() {
        let r = rig(&serves(), false);
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        let manifest = ManifestRef::new();
        let path = r
            .sources
            .start(sandbox, role, manifest, None)
            .await
            .unwrap();
        assert_eq!(path, PathBuf::from("/dev/ublkb7"));
        assert_eq!(
            r.args().unwrap().trim(),
            format!("serve --manifest {manifest} --queues 2 --depth 64")
        );
        let rec = r.sources.records().await.pop().unwrap();
        assert_eq!(rec.device.as_ref().map(|d| d.id), Some(7));
        assert_eq!((rec.queues, rec.depth), (2, 64));
        let again = r.sources.start(sandbox, role, manifest, None).await;
        assert_eq!(again.unwrap_err().kind(), io::ErrorKind::AlreadyExists);

        // The fake has no device, as after a normal stop.
        r.sources.stop(sandbox, role).await.unwrap();
        assert!(r.probe.start_time(rec.pid).is_none(), "the process exited");
        assert!(r.sources.records().await.is_empty());
        assert!(!r.sources.0.log_path(sandbox, role).exists());
        assert_eq!(r.deletes(), None, "no delete was needed");
        r.sources.stop(sandbox, role).await.unwrap();
    }

    /// The server's own delete failed after it stopped the device: the
    /// stop deletes the device without a server.
    #[tokio::test]
    async fn a_device_left_by_its_server_is_deleted_by_the_stop() {
        let r = rig(&serves(), false);
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        r.sources
            .start(sandbox, role, ManifestRef::new(), None)
            .await
            .unwrap();
        r.add_device(7);
        r.sources.stop(sandbox, role).await.unwrap();
        assert_eq!(r.deletes().unwrap().trim(), "delete --device 7 --quiesced");
        assert!(!r.probe.device_exists(7));
        assert!(r.sources.records().await.is_empty());
    }

    /// The kernel refuses a recovery of a device its server stopped and did
    /// not delete; the stop deletes it instead of failing forever.
    #[tokio::test]
    async fn a_device_that_cannot_be_recovered_is_deleted_by_the_stop() {
        let r = rig(&format!("{DELETES}\n{RECOVERY_REFUSED}\n{SERVE}"), false);
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        r.mark_dead(4242);
        r.add_device(7);
        r.persist(&record(sandbox, role, 4242, Some(7))).await;
        r.sources.ensure_alive(sandbox, role).await.unwrap_err();
        r.sources.stop(sandbox, role).await.unwrap();
        assert_eq!(r.deletes().unwrap().trim(), "delete --device 7 --quiesced");
        assert!(r.sources.records().await.is_empty());
    }

    #[tokio::test]
    async fn a_stop_whose_delete_is_refused_keeps_the_record() {
        let r = rig(&format!("{DELETE_REFUSED}\n{SERVE}"), false);
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        r.sources
            .start(sandbox, role, ManifestRef::new(), None)
            .await
            .unwrap();
        r.add_device(7);
        let err = r.sources.stop(sandbox, role).await.unwrap_err();
        assert!(err.to_string().contains("stays"), "{err}");
        assert_eq!(r.sources.list().await.unwrap(), vec![(sandbox, role)]);
        // The next stop recovers the device first, then stops it.
        std::fs::remove_file(r.dir.path().join("args")).unwrap();
        r.sources.stop(sandbox, role).await.unwrap_err();
        assert!(r.args().unwrap().contains("--recover 7"));
        std::fs::remove_file(r.probe.devices.join("7")).unwrap();
        r.sources.stop(sandbox, role).await.unwrap();
        assert!(r.sources.records().await.is_empty());
    }

    #[tokio::test]
    async fn a_start_that_does_not_serve_leaves_nothing() {
        let r = rig("exit 3", false);
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        r.sources
            .start(sandbox, role, ManifestRef::new(), None)
            .await
            .unwrap_err();
        assert!(r.sources.records().await.is_empty());
    }

    /// A caller dropped while the start waits for the ready line leaves a
    /// source that its record names, never a process without one.
    #[tokio::test]
    async fn a_dropped_start_finishes_in_its_own_task() {
        let r = rig(&format!("{DELETES}\nsleep 1\n{SERVE}"), false);
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        let start = r.sources.start(sandbox, role, ManifestRef::new(), None);
        let cut = tokio::time::timeout(Duration::from_millis(200), start).await;
        assert!(cut.is_err(), "the caller gave up");
        for _ in 0..200 {
            if r.sources.records().await.iter().any(|r| r.device.is_some()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let rec = r.sources.records().await.pop().unwrap();
        assert_eq!(rec.device.map(|d| d.id), Some(7));
        r.sources.stop(sandbox, role).await.unwrap();
    }

    #[tokio::test]
    async fn no_driver_no_start() {
        let r = rig(&serves(), true);
        assert!(!r.sources.can_start());
        let err = r
            .sources
            .start(SandboxId::new(), DiskRole::Root, ManifestRef::new(), None)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert!(err.to_string().contains("/dev/ublk-control"), "{err}");
        assert_eq!(r.args(), None);
    }

    /// The sweep deletes devices that no record names and that no process
    /// serves; the binary refuses a device a process serves.
    #[tokio::test]
    async fn the_sweep_deletes_only_unrecorded_devices() {
        let r = rig(&serves(), false);
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        r.mark_alive(4242);
        r.persist(&record(sandbox, role, 4242, Some(3))).await;
        r.add_device(3);
        r.add_device(8);
        r.probe.processes.lock().unwrap().push((5151, 1));
        r.sources.sweep_unrecorded().await;
        assert_eq!(r.deletes().unwrap().trim(), "delete --device 8 --quiesced");
        assert!(r.probe.device_exists(3), "a recorded device stays");
        assert!(!r.probe.device_exists(8));
    }

    /// A record file that cannot be read may name a device that a drive
    /// reads; the sweep then deletes only devices whose server stopped them.
    #[tokio::test]
    async fn an_unreadable_record_limits_the_sweep_to_stopped_devices() {
        let r = rig(&serves(), false);
        std::fs::create_dir_all(&r.sources.0.dir).unwrap();
        std::fs::write(r.sources.0.dir.join("torn.json"), b"{").unwrap();
        r.add_device(8);
        r.sources.sweep_unrecorded().await;
        assert_eq!(r.deletes().unwrap().trim(), "delete --device 8");
    }
}
