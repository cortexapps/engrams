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
//! A ublk device stopped or deleted with a request that no server will
//! finish blocks in the kernel until the node reboots. So only a SIGTERM
//! to a serving process deletes a device (the server fails the reads it
//! cannot serve, then deletes the device), a recovery that fails ends
//! with SIGKILL, which keeps the device for the next recovery, and a stop
//! recovers a dead source first.

use std::collections::HashMap;
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
    /// How long a process may take to exit after a signal.
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

/// One source process, as its durable record holds it.
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
    pub base: Option<Base>,
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
}

/// [`Probe`] over `/proc` and sysfs.
pub struct KernelProbe;

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
        // The control node lives from the device's add to its delete.
        Path::new(&format!("/sys/class/ublk-char/ublkc{id}")).exists()
    }

    fn driver_loaded(&self) -> bool {
        Path::new("/dev/ublk-control").exists()
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

/// [`DiskSources`] as `engram-disk-source` processes, one per role.
pub struct ProcessSources {
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
        Self {
            cfg,
            dir,
            fs,
            probe,
            locks: Default::default(),
        }
    }

    /// Whether this host can start a source; the reason when it cannot.
    pub fn check_driver(&self) -> io::Result<()> {
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

    /// Every record on this host.
    pub async fn records(&self) -> Vec<SourceRecord> {
        durable_record::load_all(self.fs.as_ref(), &self.dir, WHAT).await
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
        self.clean_up(sandbox, role).await?;
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("disk source {id}: {gone}"),
        ))
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
    async fn start(
        &self,
        sandbox: SandboxId,
        role: DiskRole,
        manifest: ManifestRef,
        base: Option<(&Volume, ManifestRef)>,
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
            base: base.map(|(volume, manifest)| Base {
                volume: volume.path.clone(),
                manifest,
            }),
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

    async fn served(&self, sandbox: SandboxId, role: DiskRole) -> io::Result<Served> {
        let record = self
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
        let _guard = self.lock((sandbox, role)).await;
        let Some(record) = self.load(sandbox, role).await? else {
            return Ok(());
        };
        let (record, device) = match self.revive(record).await {
            Ok(serving) => serving,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        };
        if !self.terminate(record.pid, record.start_time).await {
            return Err(io::Error::other(format!(
                "disk source {} (pid {}) does not exit",
                Self::id(sandbox, role),
                record.pid
            )));
        }
        if self.probe.device_exists(device.id) {
            // The next stop recovers the device and stops it again.
            return Err(io::Error::other(format!(
                "device {} of disk source {} stays after its process exited",
                device.id,
                Self::id(sandbox, role)
            )));
        }
        self.clean_up(sandbox, role).await
    }

    async fn list(&self) -> io::Result<Vec<(SandboxId, DiskRole)>> {
        Ok(self
            .records()
            .await
            .into_iter()
            .map(|r| (r.sandbox, r.role))
            .collect())
    }
}

/// Make every recorded source serve, after the pool opens and before the
/// volume reconcile, which removes drives: a drive over a dead source
/// cannot be removed.
pub async fn recover_all(sources: &dyn DiskSources) {
    let roles = match sources.list().await {
        Ok(roles) => roles,
        Err(error) => {
            tracing::warn!(%error, "disk source recovery: cannot list the sources");
            return;
        }
    };
    for (sandbox, role) in roles {
        match sources.ensure_alive(sandbox, role).await {
            Ok(()) => tracing::debug!(%sandbox, %role, "disk source serves"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                tracing::info!(%sandbox, %role, %error, "disk source removed");
            }
            Err(error) => {
                tracing::error!(%sandbox, %role, %error, "disk source recovery failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! Records and the restart decision, with a fake probe and a shell
    //! script in place of the binary; the kernel side is the root lane's
    //! (`tests/disk_source_ublk.rs`). Crash states follow ADR 0099 H5: each
    //! post-crash file state is built directly.

    use std::collections::HashSet;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    use super::*;

    /// Processes run unless the test says otherwise; devices exist only
    /// when the test adds them.
    #[derive(Default)]
    struct FakeProbe {
        start_times: Mutex<HashMap<u32, Option<u64>>>,
        devices: Mutex<HashSet<u32>>,
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
            self.devices.lock().unwrap().contains(&id)
        }
        fn driver_loaded(&self) -> bool {
            !self.no_driver
        }
    }

    struct Rig {
        dir: tempfile::TempDir,
        probe: Arc<FakeProbe>,
        sources: ProcessSources,
    }

    /// `script` is the body of the fake binary; `$ARGS` names a file it
    /// may write its arguments to.
    fn rig(script: &str, probe: FakeProbe) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake-disk-source");
        let args = dir.path().join("args");
        let body = script.replace("$ARGS", &args.display().to_string());
        std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut cfg = ProcessSourcesConfig::new(bin, dir.path());
        cfg.ready_timeout = Duration::from_secs(10);
        cfg.stop_timeout = Duration::from_secs(5);
        let probe = Arc::new(probe);
        let sources = ProcessSources::with_probe(cfg, Arc::new(TokioFs), probe.clone());
        Rig {
            dir,
            probe,
            sources,
        }
    }

    const SERVES: &str = "echo \"$@\" > $ARGS\necho /dev/ublkb7 7\nexec sleep 60";

    impl Rig {
        fn args(&self) -> Option<String> {
            std::fs::read_to_string(self.dir.path().join("args")).ok()
        }
        fn record_path(&self, sandbox: SandboxId, role: DiskRole) -> PathBuf {
            durable_record::record_path(&self.sources.dir, ProcessSources::id(sandbox, role))
        }
        fn mark_dead(&self, pid: u32) {
            self.probe.start_times.lock().unwrap().insert(pid, None);
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

    #[tokio::test]
    async fn a_record_round_trips_and_names_what_the_source_serves() {
        let r = rig(SERVES, FakeProbe::default());
        let (sandbox, role) = (SandboxId::new(), DiskRole::Swap);
        let rec = record(sandbox, role, 4242, Some(5));
        r.sources.persist(&rec).await.unwrap();

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
        let r = rig(SERVES, FakeProbe::default());
        let (torn, sibling) = (SandboxId::new(), SandboxId::new());
        let role = DiskRole::Root;
        r.sources
            .persist(&record(sibling, role, 1, Some(1)))
            .await
            .unwrap();
        r.sources
            .persist(&record(torn, role, 2, Some(2)))
            .await
            .unwrap();
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
        let r = rig(SERVES, FakeProbe::default());
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
        let r = rig(SERVES, FakeProbe::default());
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        let rec = record(sandbox, role, 1, Some(1));
        r.sources.persist(&rec).await.unwrap();
        let dir = &r.sources.dir;
        // A crash before the rename, a log, and junk.
        let other = record(SandboxId::new(), role, 2, Some(2));
        let other_id = ProcessSources::id(other.sandbox, role);
        let sealed =
            crate::durable_envelope::seal(&other_id, &serde_json::to_string(&other).unwrap());
        std::fs::write(dir.join(format!("{other_id}.json.partial")), sealed).unwrap();
        std::fs::write(r.sources.log_path(sandbox, role), b"x").unwrap();
        std::fs::write(dir.join("junk.json"), b"\x00\xff not json").unwrap();
        assert_eq!(r.sources.records().await, vec![rec]);
    }

    #[tokio::test]
    async fn a_live_source_is_left_alone() {
        let r = rig(SERVES, FakeProbe::default());
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        let rec = record(sandbox, role, 4242, Some(5));
        r.probe.start_times.lock().unwrap().insert(4242, Some(1));
        r.sources.persist(&rec).await.unwrap();
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
            let r = rig(SERVES, FakeProbe::default());
            let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
            let rec = record(sandbox, role, 4242, Some(7));
            let time = if reused { Some(99) } else { None };
            r.probe.start_times.lock().unwrap().insert(4242, time);
            r.probe.devices.lock().unwrap().insert(7);
            r.sources.persist(&rec).await.unwrap();

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
        let r = rig(SERVES, FakeProbe::default());
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        r.mark_dead(4242);
        r.sources
            .persist(&record(sandbox, role, 4242, Some(7)))
            .await
            .unwrap();
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
        let r = rig(SERVES, FakeProbe::default());
        let role = DiskRole::Root;
        let (stale, owner) = (SandboxId::new(), SandboxId::new());
        r.mark_dead(4242);
        r.probe.start_times.lock().unwrap().insert(4343, Some(1));
        r.probe.devices.lock().unwrap().insert(7);
        r.sources
            .persist(&record(stale, role, 4242, Some(7)))
            .await
            .unwrap();
        let held = record(owner, role, 4343, Some(7));
        r.sources.persist(&held).await.unwrap();

        let err = r.sources.ensure_alive(stale, role).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(r.sources.records().await, vec![held]);
        assert_eq!(r.args(), None);
    }

    #[tokio::test]
    async fn a_start_that_never_reported_a_device_is_removed() {
        let r = rig(SERVES, FakeProbe::default());
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        r.mark_dead(4242);
        r.sources
            .persist(&record(sandbox, role, 4242, None))
            .await
            .unwrap();
        let err = r.sources.ensure_alive(sandbox, role).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(r.sources.records().await.is_empty());
    }

    #[tokio::test]
    async fn start_records_the_device_and_stop_removes_every_trace() {
        let r = rig(SERVES, FakeProbe::default());
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
        assert!(!r.sources.log_path(sandbox, role).exists());
        r.sources.stop(sandbox, role).await.unwrap();
    }

    #[tokio::test]
    async fn a_stop_that_leaves_the_device_keeps_the_record() {
        let r = rig(SERVES, FakeProbe::default());
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        r.sources
            .start(sandbox, role, ManifestRef::new(), None)
            .await
            .unwrap();
        r.probe.devices.lock().unwrap().insert(7);
        let err = r.sources.stop(sandbox, role).await.unwrap_err();
        assert!(err.to_string().contains("stays"), "{err}");
        assert_eq!(r.sources.list().await.unwrap(), vec![(sandbox, role)]);
        // The next stop recovers the device, then stops it.
        std::fs::remove_file(r.dir.path().join("args")).unwrap();
        r.sources.stop(sandbox, role).await.unwrap_err();
        assert!(r.args().unwrap().contains("--recover 7"));
        r.probe.devices.lock().unwrap().clear();
        r.sources.stop(sandbox, role).await.unwrap();
        assert!(r.sources.records().await.is_empty());
    }

    #[tokio::test]
    async fn a_start_that_does_not_serve_leaves_nothing() {
        let r = rig("exit 3", FakeProbe::default());
        let (sandbox, role) = (SandboxId::new(), DiskRole::Root);
        r.sources
            .start(sandbox, role, ManifestRef::new(), None)
            .await
            .unwrap_err();
        assert!(r.sources.records().await.is_empty());
    }

    #[tokio::test]
    async fn no_driver_no_start() {
        let probe = FakeProbe {
            no_driver: true,
            ..Default::default()
        };
        let r = rig(SERVES, probe);
        let err = r
            .sources
            .start(SandboxId::new(), DiskRole::Root, ManifestRef::new(), None)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert!(err.to_string().contains("/dev/ublk-control"), "{err}");
        assert_eq!(r.args(), None);
    }
}
