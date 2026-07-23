//! Durable, attachable exec records (ADR 0103).
//!
//! The directory itself is the spawn-deduplication marker. Everything else
//! is crash-tolerant: `exit.json` is the sole completeness marker, its
//! `.tmp` sibling is never trusted, and unknown siblings are ignored.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;

pub const DEFAULT_EXEC_JOURNAL_ROOT: &str = "/var/lib/engram/execs";
pub const OUTPUT_CAP_BYTES: u64 = 64 * 1024 * 1024;
pub const OUTPUT_TRUNCATED_MARKER: &[u8] = b"\n[engram: output truncated at 64 MiB]\n";
const DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const DEFAULT_MAX_ACTIVE: usize = 32;

#[derive(Clone, Debug)]
pub struct ExecJournal {
    root: PathBuf,
    ttl: Duration,
    max_active: usize,
}

#[derive(Clone, Debug)]
pub struct JournalEntry {
    dir: PathBuf,
    exec_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequestRecord {
    pub command: Vec<String>,
    pub created_at_unix_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExitRecord {
    pub exit: Option<i32>,
    pub finished_at_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachState {
    Running,
    Died { reason: String },
    Complete(ExitRecord),
    Mismatch { recorded_command: Vec<String> },
}

#[derive(Clone, Debug)]
pub enum AttachOrStart {
    Start(JournalEntry),
    Attach(JournalEntry),
    Missing,
    /// The atomic marker or request record could not be persisted. The
    /// command still runs, but only the live stage-1 stream is authoritative.
    DegradedStart {
        entry: JournalEntry,
        reason: String,
    },
}

impl ExecJournal {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            ttl: DEFAULT_TTL,
            max_active: DEFAULT_MAX_ACTIVE,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub async fn attach_or_start(
        &self,
        exec_id: &str,
        command: &[String],
    ) -> io::Result<AttachOrStart> {
        validate_exec_id(exec_id)?;
        if let Err(error) = tokio::fs::create_dir_all(&self.root).await {
            return Ok(AttachOrStart::DegradedStart {
                entry: JournalEntry::new(self.root.join(exec_id), exec_id),
                reason: format!("create journal root: {error}"),
            });
        }
        self.gc_expired().await;

        let entry = JournalEntry::new(self.root.join(exec_id), exec_id);
        match tokio::fs::create_dir(entry.dir()).await {
            Ok(()) => {
                if self.active_count().await > self.max_active {
                    let reason = format!(
                        "concurrent journal cap ({}) reached; durable recording disabled",
                        self.max_active
                    );
                    entry.mark_degraded(&reason).await;
                    return Ok(AttachOrStart::DegradedStart { entry, reason });
                }
                let request = RequestRecord {
                    command: command.to_vec(),
                    created_at_unix_ms: unix_ms(),
                };
                if let Err(error) = entry.write_request(&request).await {
                    let reason = format!("write request.json: {error}");
                    entry.mark_degraded(&reason).await;
                    return Ok(AttachOrStart::DegradedStart { entry, reason });
                }
                Ok(AttachOrStart::Start(entry))
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Ok(AttachOrStart::Attach(entry))
            }
            Err(error) => Ok(AttachOrStart::DegradedStart {
                entry,
                reason: format!("mkdir spawn marker: {error}"),
            }),
        }
    }

    pub async fn attach_existing(&self, exec_id: &str) -> io::Result<AttachOrStart> {
        validate_exec_id(exec_id)?;
        let entry = JournalEntry::new(self.root.join(exec_id), exec_id);
        match tokio::fs::metadata(entry.dir()).await {
            Ok(metadata) if metadata.is_dir() => Ok(AttachOrStart::Attach(entry)),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "exec journal marker is not a directory",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(AttachOrStart::Missing),
            Err(error) => Err(error),
        }
    }

    /// Records that are actually recording: no completion or degraded
    /// marker AND a live wrapper/command (or in-flight spawn). A wrapper
    /// killed without writing `exit.json` is diagnosable but must not
    /// occupy a cap slot forever — 32 unclean deaths would otherwise turn
    /// every later exec on a long-lived session into a silent
    /// `DegradedStart`, disabling durability with no signal.
    async fn active_count(&self) -> usize {
        let Ok(mut entries) = tokio::fs::read_dir(&self.root).await else {
            return 0;
        };
        let mut count = 0;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(kind) = entry.file_type().await else {
                continue;
            };
            if !kind.is_dir()
                || tokio::fs::metadata(entry.path().join("exit.json"))
                    .await
                    .is_ok()
                || tokio::fs::metadata(entry.path().join("degraded"))
                    .await
                    .is_ok()
            {
                continue;
            }
            // Non-journal garbage (invalid exec_id names) was never started
            // by us and holds no slot.
            let Ok(journal) = JournalEntry::from_dir(entry.path()) else {
                continue;
            };
            if journal.appears_live().await {
                count += 1;
            }
        }
        count
    }

    /// Best-effort TTL collection. Completed records age from their
    /// `exit.json`; records whose wrapper is provably dead (died-without-
    /// marker, degraded, torn) age from their request record or, failing
    /// that, the directory mtime. Both stay diagnosable for a full TTL
    /// before reclamation, so incomplete records are never mistaken for
    /// completed work — but they also cannot accumulate disk without bound.
    /// A live recording is never collected, whatever its age.
    pub async fn gc_expired(&self) {
        let Ok(mut entries) = tokio::fs::read_dir(&self.root).await else {
            return;
        };
        let now = unix_ms();
        let ttl_ms = self.ttl.as_millis() as u64;
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(kind) = entry.file_type().await else {
                continue;
            };
            if !kind.is_dir() {
                continue;
            }
            let exit_path = entry.path().join("exit.json");
            let age_ms = match tokio::fs::read(&exit_path).await {
                Ok(bytes) => match serde_json::from_slice::<ExitRecord>(&bytes) {
                    Ok(exit) => now.saturating_sub(exit.finished_at_unix_ms),
                    // Corrupt exit marker: classify by liveness like any
                    // other incomplete record below.
                    Err(_) => match Self::dead_record_age_ms(entry.path(), now).await {
                        Some(age) => age,
                        None => continue,
                    },
                },
                Err(_) => match Self::dead_record_age_ms(entry.path(), now).await {
                    Some(age) => age,
                    None => continue,
                },
            };
            if age_ms >= ttl_ms {
                if let Err(error) = tokio::fs::remove_dir_all(entry.path()).await {
                    tracing::warn!(path = %entry.path().display(), %error, "exec journal TTL GC failed");
                }
            }
        }
    }

    /// Age of an incomplete record that is provably not recording any more,
    /// or `None` when it is live (or unparseable in a way that can't prove
    /// death) and must be retained.
    async fn dead_record_age_ms(dir: PathBuf, now: u64) -> Option<u64> {
        let journal = JournalEntry::from_dir(&dir).ok()?;
        if journal.appears_live().await {
            return None;
        }
        match journal.request().await {
            Ok(request) => Some(now.saturating_sub(request.created_at_unix_ms)),
            Err(_) => {
                let modified = tokio::fs::metadata(&dir)
                    .await
                    .ok()
                    .and_then(|meta| meta.modified().ok())?;
                Some(modified.elapsed().ok()?.as_millis() as u64)
            }
        }
    }
}

impl Default for ExecJournal {
    fn default() -> Self {
        #[cfg(test)]
        return Self::new("/dev/null/engram-execs");

        #[cfg(not(test))]
        Self::new(DEFAULT_EXEC_JOURNAL_ROOT)
    }
}

impl JournalEntry {
    fn new(dir: PathBuf, exec_id: &str) -> Self {
        Self {
            dir,
            exec_id: exec_id.to_string(),
        }
    }

    pub fn from_dir(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        let exec_id = dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "journal has no exec id"))?;
        validate_exec_id(exec_id)?;
        Ok(Self::new(dir.clone(), exec_id))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn exec_id(&self) -> &str {
        &self.exec_id
    }

    pub fn stdout_path(&self) -> PathBuf {
        self.dir.join("stdout")
    }

    pub fn stderr_path(&self) -> PathBuf {
        self.dir.join("stderr")
    }

    pub async fn write_request(&self, request: &RequestRecord) -> io::Result<()> {
        atomic_write_json(&self.dir.join("request.json"), request).await
    }

    pub async fn request(&self) -> io::Result<RequestRecord> {
        read_json(&self.dir.join("request.json")).await
    }

    pub async fn write_pid(&self, pid: u32) -> io::Result<()> {
        atomic_write(&self.dir.join("pid"), pid.to_string().as_bytes()).await
    }

    pub async fn write_owner_pid(&self, pid: u32) -> io::Result<()> {
        atomic_write(&self.dir.join("owner_pid"), pid.to_string().as_bytes()).await
    }

    pub async fn pid(&self) -> io::Result<u32> {
        let raw = tokio::fs::read_to_string(self.dir.join("pid")).await?;
        raw.trim()
            .parse::<u32>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    async fn owner_pid(&self) -> io::Result<u32> {
        let raw = tokio::fs::read_to_string(self.dir.join("owner_pid")).await?;
        raw.trim()
            .parse::<u32>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    pub async fn exit(&self) -> io::Result<ExitRecord> {
        read_json(&self.dir.join("exit.json")).await
    }

    pub async fn finish(&self, exit: Option<i32>) -> io::Result<()> {
        atomic_write_json(
            &self.dir.join("exit.json"),
            &ExitRecord {
                exit,
                finished_at_unix_ms: unix_ms(),
            },
        )
        .await
    }

    pub async fn state_for(&self, command: &[String]) -> AttachState {
        let request = match self.request().await {
            Ok(request) => request,
            Err(error) => {
                return AttachState::Died {
                    reason: format!("request.json missing or corrupt: {error}"),
                };
            }
        };
        if request.command != command {
            return AttachState::Mismatch {
                recorded_command: request.command,
            };
        }
        match self.exit().await {
            Ok(exit) => return AttachState::Complete(exit),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return AttachState::Died {
                    reason: format!("exit.json corrupt: {error}"),
                };
            }
        }
        // `pid` is the cancellable command process-group leader, which can
        // exit before the wrapper has drained its pipes and renamed
        // exit.json. The wrapper's own pid closes that normal completion
        // race: while `owner_pid` is alive, absence of exit.json means
        // Running even when the command pid is already dead.
        match self.owner_pid().await {
            Ok(pid) if process_is_alive(pid) => return AttachState::Running,
            Ok(pid) => {
                return AttachState::Died {
                    reason: format!("wrapper pid {pid} is dead and exit.json is absent"),
                };
            }
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                return AttachState::Died {
                    reason: format!("owner_pid corrupt and exit.json is absent: {error}"),
                };
            }
            Err(_) => {}
        }
        match self.pid().await {
            Ok(pid) if process_is_alive(pid) => AttachState::Running,
            Ok(pid) => AttachState::Died {
                reason: format!("pid {pid} is dead and exit.json is absent"),
            },
            // The mkdir/request write happens before fork and the wrapper
            // writes pid immediately after spawning. Treat this tiny window
            // as running; a later poll turns a permanently missing pid into a
            // loud died-without-marker result.
            Err(_error) if unix_ms().saturating_sub(request.created_at_unix_ms) < 5_000 => {
                AttachState::Running
            }
            Err(error) => AttachState::Died {
                reason: format!("pid missing or corrupt and exit.json is absent: {error}"),
            },
        }
    }

    /// Command-independent liveness for cap accounting and GC: is this
    /// record's wrapper or command actually running (or still inside the
    /// mkdir→write_pid spawn window)? Mirrors `state_for`'s ladder without
    /// the request/command comparison. PID reuse can read a dead wrapper as
    /// alive — an over-count, which is the safe direction for a resource cap
    /// and merely delays GC by one reuse lifetime.
    async fn appears_live(&self) -> bool {
        match self.owner_pid().await {
            Ok(pid) => return process_is_alive(pid),
            Err(error) if error.kind() != io::ErrorKind::NotFound => return false,
            Err(_) => {}
        }
        match self.pid().await {
            Ok(pid) => process_is_alive(pid),
            Err(error) if error.kind() != io::ErrorKind::NotFound => false,
            Err(_) => {
                // Neither pid has landed. Within the spawn window that is a
                // live start (the cap check in `attach_or_start` runs before
                // `write_request`, so the dir under authorization has neither
                // request nor pids); past the same 5s grace `state_for`
                // uses, it is a dead torn record.
                let since_created = match self.request().await {
                    Ok(request) => unix_ms().saturating_sub(request.created_at_unix_ms),
                    Err(_) => {
                        let Some(modified) = tokio::fs::metadata(&self.dir)
                            .await
                            .ok()
                            .and_then(|meta| meta.modified().ok())
                        else {
                            return false;
                        };
                        match modified.elapsed() {
                            Ok(elapsed) => elapsed.as_millis() as u64,
                            // Dir mtime in the future (clock step): treat as
                            // just-created rather than reaping a live start.
                            Err(_) => 0,
                        }
                    }
                };
                since_created < 5_000
            }
        }
    }

    pub async fn mark_degraded(&self, reason: &str) {
        let _ = atomic_write(&self.dir.join("degraded"), reason.as_bytes()).await;
    }

    pub async fn degraded_reason(&self) -> io::Result<String> {
        tokio::fs::read_to_string(self.dir.join("degraded")).await
    }
}

/// Hidden child mode run by the exec wrapper process. It owns the command,
/// capped journal files, and the atomic exit marker; agentd may execve a new
/// generation without affecting this process.
pub async fn run_wrapper(
    entry: JournalEntry,
    command: Vec<String>,
    timeout: Option<Duration>,
) -> io::Result<Option<i32>> {
    if let Err(error) = entry.write_owner_pid(std::process::id()).await {
        entry
            .mark_degraded(&format!("write wrapper owner pid: {error}"))
            .await;
    }
    let program = command.first().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "exec wrapper command is empty")
    })?;
    let mut child_cmd = Command::new(program);
    child_cmd
        .args(&command[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    #[cfg(unix)]
    {
        child_cmd.process_group(0);
    }
    let mut child = child_cmd.spawn()?;
    if let Some(pid) = child.id() {
        if let Err(error) = entry.write_pid(pid).await {
            entry.mark_degraded(&format!("write pid: {error}")).await;
        }
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("exec wrapper stdout pipe missing"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("exec wrapper stderr pipe missing"))?;
    let out_entry = entry.clone();
    let err_entry = entry.clone();
    let out_task = tokio::spawn(capture_output(
        stdout,
        tokio::io::stdout(),
        out_entry.stdout_path(),
        out_entry,
    ));
    let err_task = tokio::spawn(capture_output(
        stderr,
        tokio::io::stderr(),
        err_entry.stderr_path(),
        err_entry,
    ));

    let status = match timeout {
        Some(timeout) => match tokio::time::timeout(timeout, child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                let _ = kill_process_group(child.id());
                let _ = child.start_kill();
                child.wait().await?
            }
        },
        None => child.wait().await?,
    };
    let _ = out_task.await;
    let _ = err_task.await;
    let exit = status.code();
    if let Err(error) = entry.finish(exit).await {
        entry
            .mark_degraded(&format!("write exit completeness marker: {error}"))
            .await;
    }
    Ok(exit)
}

async fn capture_output<R, W>(mut reader: R, mut live: W, path: PathBuf, entry: JournalEntry)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut file = match tokio::fs::File::create(&path).await {
        Ok(file) => Some(file),
        Err(error) => {
            entry
                .mark_degraded(&format!("create {}: {error}", path.display()))
                .await;
            None
        }
    };
    let data_cap = OUTPUT_CAP_BYTES.saturating_sub(OUTPUT_TRUNCATED_MARKER.len() as u64);
    let mut persisted = 0u64;
    let mut truncated = false;
    let mut live_open = true;
    let mut buffer = vec![0u8; 8 * 1024];
    loop {
        let count = match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(count) => count,
            Err(_) => break,
        };
        if live_open && live.write_all(&buffer[..count]).await.is_err() {
            live_open = false;
        }
        let Some(output) = file.as_mut() else {
            continue;
        };
        if persisted < data_cap {
            let remaining = (data_cap - persisted) as usize;
            let keep = remaining.min(count);
            if let Err(error) = output.write_all(&buffer[..keep]).await {
                entry
                    .mark_degraded(&format!("write {}: {error}", path.display()))
                    .await;
                file = None;
                continue;
            }
            persisted += keep as u64;
            if keep < count {
                truncated = true;
            }
        } else {
            truncated = true;
        }
        if truncated {
            if let Err(error) = output.write_all(OUTPUT_TRUNCATED_MARKER).await {
                entry
                    .mark_degraded(&format!(
                        "write truncation marker {}: {error}",
                        path.display()
                    ))
                    .await;
            }
            file = None;
        }
    }
    if let Some(mut output) = file {
        let _ = output.flush().await;
        let _ = output.sync_all().await;
    }
}

pub async fn cancel(entry: &JournalEntry) -> io::Result<()> {
    let mut last_error = None;
    for _ in 0..10 {
        match entry.pid().await {
            Ok(pid) => {
                kill_process_group(Some(pid))?;
                return Ok(());
            }
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(last_error.unwrap_or_else(|| io::Error::new(io::ErrorKind::NotFound, "pid absent")))
}

fn validate_exec_id(exec_id: &str) -> io::Result<()> {
    let valid = !exec_id.is_empty()
        && exec_id.len() <= 200
        && !exec_id.starts_with('.')
        && !exec_id.starts_with("__engram_")
        && exec_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'));
    if valid {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "exec_id must be 1..=200 ASCII [A-Za-z0-9_.:-] bytes, not start with '.', and not use the reserved __engram_ prefix",
        ))
    }
}

async fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<T> {
    let bytes = tokio::fs::read(path).await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

async fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    atomic_write(path, &bytes).await
}

async fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("record")
    ));
    let mut file = tokio::fs::File::create(&tmp).await?;
    file.write_all(bytes).await?;
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(tmp, path).await
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(target_os = "linux")]
fn process_is_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), None).is_ok()
}

#[cfg(not(target_os = "linux"))]
fn process_is_alive(pid: u32) -> bool {
    pid == std::process::id()
}

#[cfg(target_os = "linux")]
fn kill_process_group(pid: Option<u32>) -> io::Result<()> {
    let pid = pid
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid process-group pid"))?;
    nix::sys::signal::killpg(
        nix::unistd::Pid::from_raw(pid),
        nix::sys::signal::Signal::SIGKILL,
    )
    .map_err(|error| io::Error::from_raw_os_error(error as i32))
}

#[cfg(not(target_os = "linux"))]
fn kill_process_group(_pid: Option<u32>) -> io::Result<()> {
    Ok(())
}
