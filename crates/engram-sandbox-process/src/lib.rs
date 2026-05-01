//! Local-dev `SandboxBackend` that runs commands as plain host
//! subprocesses, with each "sandbox" rooted at its own working
//! directory under `work_dir`.
//!
//! # Safety / scope
//!
//! **There is no isolation here.** This backend exists so the entire
//! orchestration layer (coordinator API, scheduler, snapshot manager,
//! warm pool, blob storage, host-agent loops) can be exercised on a
//! developer's laptop — including macOS Apple Silicon, where the
//! production VMM (Firecracker, KVM-only) cannot run. Real isolation
//! comes from `engram-sandbox-firecracker` on Linux production hosts.
//!
//! NEVER use this backend with untrusted input or in any deployment.
//! It runs whatever shell command it's given as the same user as the
//! host agent.
//!
//! # Snapshot format
//!
//! `snapshot()` writes two files into `dest`:
//!
//! - `manifest.json` — a small record of the sandbox spec + image
//!   version + sandbox id (for sanity-checking on restore).
//! - `fs.tar.gz` — the sandbox's working directory, gzip-compressed
//!   tarball.
//!
//! `restore()` is the inverse: read manifest, allocate a fresh
//! sandbox id, untar `fs.tar.gz` into the new sandbox's working dir.
//! In-memory state is not preserved (subprocesses are not microVMs),
//! but the on-disk workspace round-trips, which is enough to exercise
//! the snapshot manager pipeline (LRU, replication, eviction).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use dashmap::DashMap;
use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::ids::{SandboxId, SnapshotId};
use engram_core::types::sandbox::{AgentSpec, ExecEvent, ExecRequest, ExecStream, SandboxSpec};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::SandboxError;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// POSIX `timeout(1)` convention: command was killed because it
/// exceeded its wall-clock budget. Surfaced as the exit status of an
/// `ExecEvent::Exit` in this case so callers can distinguish a slow
/// command from a normal nonzero exit.
const EXIT_CODE_TIMEOUT: i32 = 124;

/// Per-sandbox state owned by the backend.
#[derive(Clone, Debug)]
struct SandboxState {
    spec: SandboxSpec,
    cwd: PathBuf,
}

pub struct ProcessBackend {
    work_dir: PathBuf,
    sandboxes: DashMap<SandboxId, SandboxState>,
    /// Long-running agent process per sandbox (the harness adapter).
    /// Tracked separately from `sandboxes` because `tokio::process::Child`
    /// isn't `Clone`. Populated at `start_agent()`; killed at `destroy()`.
    agent_children: DashMap<SandboxId, std::sync::Mutex<Option<tokio::process::Child>>>,
}

impl ProcessBackend {
    pub fn new(work_dir: impl Into<PathBuf>) -> Self {
        Self {
            work_dir: work_dir.into(),
            sandboxes: DashMap::new(),
            agent_children: DashMap::new(),
        }
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    fn cwd_for(&self, id: SandboxId) -> PathBuf {
        self.work_dir.join(id.to_string())
    }
}

#[async_trait]
impl SandboxBackend for ProcessBackend {
    fn harness_dial(&self) -> engram_core::traits::HarnessDial {
        // Harness exec'd as a host subprocess; dials TCP loopback
        // back to the coord-side harness listener.
        engram_core::traits::HarnessDial::HostTcp
    }

    fn supports_local_mount(&self) -> bool {
        // Process backend "mounts" via host symlinks — trivially
        // supported since the sandbox cwd is a host directory.
        true
    }

    async fn create(&self, spec: SandboxSpec) -> Result<SandboxId, SandboxError> {
        let id = SandboxId::new();
        let cwd = self.cwd_for(id);
        tokio::fs::create_dir_all(&cwd).await?;
        if let Some(src) = spec.rootfs_source.as_deref() {
            materialize_rootfs(src, &cwd)
                .await
                .map_err(|e| SandboxError::Vm(format!("materialize rootfs: {e}").into()))?;
        }
        self.sandboxes.insert(id, SandboxState { spec, cwd });
        Ok(id)
    }

    async fn start_agent(&self, id: SandboxId, agent: AgentSpec) -> Result<(), SandboxError> {
        let state = self
            .sandboxes
            .get(&id)
            .ok_or(SandboxError::NotFound)?
            .clone();
        // Idempotent: a second call with the agent already running
        // is a no-op. The warm-pool path may call start_agent more
        // than once if the same sandbox is resumed-then-checkpointed-
        // then-resumed; we keep the first agent.
        if self.agent_children.contains_key(&id) {
            return Ok(());
        }
        spawn_agent(&self.agent_children, id, &agent, &state.spec.env, &state.cwd).await
    }

    async fn exec_stream(
        &self,
        id: SandboxId,
        req: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        let state = self
            .sandboxes
            .get(&id)
            .ok_or(SandboxError::NotFound)?
            .clone();
        let argv = req
            .command
            .first()
            .cloned()
            .ok_or_else(|| SandboxError::InvalidSpec("argv must not be empty".into()))?;
        let workdir = match req.workdir.as_ref() {
            Some(rel) => state.cwd.join(rel),
            None => state.cwd.clone(),
        };

        let mut env = state.spec.env.clone();
        env.extend(req.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        // PATH is *not* inherited if env is non-empty otherwise — keep
        // the host PATH so simple commands like `echo` resolve. Callers
        // that want full hermeticity can clear it explicitly.
        if !env.contains_key("PATH") {
            if let Ok(p) = std::env::var("PATH") {
                env.insert("PATH".into(), p);
            }
        }

        let mut cmd = Command::new(&argv);
        cmd.args(req.command.iter().skip(1))
            .current_dir(&workdir)
            .env_clear()
            .envs(env_iter(&env))
            .stdin(if req.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| SandboxError::Vm(format!("spawn `{argv}`: {e}").into()))?;

        if let Some(stdin) = req.stdin.as_deref() {
            if let Some(mut sink) = child.stdin.take() {
                sink.write_all(stdin).await?;
            }
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| SandboxError::Vm("stdout pipe missing".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| SandboxError::Vm("stderr pipe missing".into()))?;

        // Channel feeding the public stream. Capacity is small —
        // backpressure on the readers if a client is slow to consume.
        let (tx, rx) = mpsc::channel::<ExecEvent>(32);

        let stdout_handle = tokio::spawn(pipe_to_channel(
            stdout,
            tx.clone(),
            ExecEvent::Stdout as fn(Bytes) -> ExecEvent,
        ));
        let stderr_handle = tokio::spawn(pipe_to_channel(
            stderr,
            tx.clone(),
            ExecEvent::Stderr as fn(Bytes) -> ExecEvent,
        ));

        let timeout = req
            .timeout
            .or(state.spec.ttl)
            .unwrap_or(Duration::from_secs(60 * 60));

        // Waiter task: wait for the child (or timeout), then for the
        // pipe readers to drain, then publish the terminal Exit event.
        // Dropping `tx` after that closes the receiver end, which is
        // what terminates the public stream.
        tokio::spawn(async move {
            let exit_status = match tokio::time::timeout(timeout, child.wait()).await {
                Ok(Ok(status)) => status.code(),
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "child wait failed");
                    None
                }
                Err(_) => {
                    // Timeout: kill_on_drop fires when `child` is
                    // dropped at the end of this scope, but be explicit
                    // so the readers see EOF promptly.
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    Some(EXIT_CODE_TIMEOUT)
                }
            };

            // Drain the readers so all stdout/stderr is delivered
            // *before* the terminal Exit event.
            let _ = stdout_handle.await;
            let _ = stderr_handle.await;
            let _ = tx.send(ExecEvent::Exit(exit_status)).await;
        });

        Ok(ExecStream {
            sandbox_id: id,
            exec_id: SandboxId::new().to_string(),
            events: Box::pin(ReceiverStream::new(rx)),
        })
    }

    async fn snapshot(&self, id: SandboxId, dest: &Path) -> Result<SnapshotMetadata, SandboxError> {
        let state = self
            .sandboxes
            .get(&id)
            .ok_or(SandboxError::NotFound)?
            .clone();
        tokio::fs::create_dir_all(dest).await?;

        let manifest_path = dest.join("manifest.json");
        let manifest = Manifest {
            sandbox_id: id,
            image_version: state.spec.image.clone(),
            created_at: Utc::now(),
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| SandboxError::Snapshot(format!("manifest serialize: {e}")))?;
        tokio::fs::write(&manifest_path, manifest_bytes).await?;

        let fs_archive = dest.join("fs.tar.gz");
        // tar+gzip is sync — run on a blocking thread to keep the
        // tokio reactor responsive.
        let cwd = state.cwd.clone();
        let archive_path = fs_archive.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
            let f = std::fs::File::create(&archive_path)?;
            let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
            let mut builder = tar::Builder::new(gz);
            builder.append_dir_all(".", &cwd)?;
            builder.into_inner()?.finish()?;
            std::fs::metadata(&archive_path).map(|m| m.len())
        })
        .await
        .map_err(|e| SandboxError::Snapshot(format!("snapshot tar join: {e}")))?
        .map_err(|e| SandboxError::Snapshot(format!("snapshot tar: {e}")))?;

        let manifest_size = tokio::fs::metadata(&manifest_path).await?.len();
        let archive_size = tokio::fs::metadata(&fs_archive).await?.len();

        Ok(SnapshotMetadata {
            id: SnapshotId::new(),
            size_bytes: manifest_size + archive_size,
            created_at: manifest.created_at,
            image_version: manifest.image_version,
        })
    }

    /// `restore` returns a *new* SandboxId pointing at a freshly-untarred
    /// cwd. This re-creates a SandboxSpec; we don't carry resource
    /// limits or rootfs_source through a snapshot because:
    ///   - Resource limits aren't enforced by ProcessBackend anyway
    ///     (set them at the next exec via the request).
    ///   - `rootfs_source` is irrelevant after restore — the cwd is
    ///     materialized from the snapshot's tarball, not from a source
    ///     image directory.
    async fn restore(&self, src: PathBuf) -> Result<SandboxId, SandboxError> {
        let manifest_bytes = tokio::fs::read(src.join("manifest.json")).await?;
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| SandboxError::Snapshot(format!("manifest parse: {e}")))?;

        // Restore into a *fresh* sandbox id rather than reusing
        // manifest.sandbox_id — different sandbox, same on-disk state.
        let id = SandboxId::new();
        let cwd = self.cwd_for(id);
        tokio::fs::create_dir_all(&cwd).await?;

        let archive = src.join("fs.tar.gz");
        let dest_cwd = cwd.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let f = std::fs::File::open(&archive)?;
            let gz = flate2::read::GzDecoder::new(f);
            let mut ar = tar::Archive::new(gz);
            ar.unpack(&dest_cwd)
        })
        .await
        .map_err(|e| SandboxError::Snapshot(format!("restore tar join: {e}")))?
        .map_err(|e| SandboxError::Snapshot(format!("restore tar: {e}")))?;

        // Synthesize a SandboxSpec from the manifest. We don't carry
        // CPU/memory/etc. through the snapshot — the next launch is a
        // fresh process, so those don't apply. `rootfs_source` is also
        // dropped: the cwd was just rehydrated from the snapshot's
        // tarball, not from a source image directory.
        let spec = SandboxSpec {
            image: manifest.image_version,
            rootfs_source: None,
            cpu: engram_core::types::sandbox::CpuLimit { vcpus: 1 },
            memory: engram_core::types::sandbox::MemoryLimit { max_mib: 0 },
            disk: engram_core::types::sandbox::DiskLimit { max_gib: 0 },
            ttl: None,
            env: HashMap::new(),
            workdir: None,
            mounts: Vec::new(),
        };
        self.sandboxes.insert(id, SandboxState { spec, cwd });
        Ok(id)
    }

    async fn destroy(&self, id: SandboxId) -> Result<(), SandboxError> {
        // Kill the agent first (if any) so it can't keep writing into
        // the cwd we're about to delete. SIGKILL via `Child::kill` —
        // the dev harness has no ack-shutdown protocol on this
        // path; production harnesses get a graceful Shutdown command
        // via the harness channel before destroy is called.
        if let Some((_, slot)) = self.agent_children.remove(&id) {
            // Take the Child out from under the std::sync::Mutex
            // (which is !Send across .await) before awaiting wait().
            let taken = { slot.lock().unwrap().take() };
            if let Some(mut child) = taken {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
        if let Some((_, state)) = self.sandboxes.remove(&id) {
            // Best-effort cleanup; if the cwd disappeared between create
            // and destroy, that's fine.
            let _ = tokio::fs::remove_dir_all(&state.cwd).await;
        }
        Ok(())
    }

    async fn list(&self) -> Result<Vec<SandboxId>, SandboxError> {
        Ok(self.sandboxes.iter().map(|r| *r.key()).collect())
    }

    /// Process-backend "guests" share the host's network stack —
    /// anything ttyd binds is reachable on localhost. Doesn't bother
    /// confirming the sandbox is alive; the caller deals with the
    /// connection failure if it isn't.
    async fn guest_ip(&self, id: SandboxId) -> Option<String> {
        if self.sandboxes.contains_key(&id) {
            Some("127.0.0.1".to_string())
        } else {
            None
        }
    }
}

/// Spawn the sandbox's long-running agent process (harness adapter,
/// noop dev harness, etc.). Stdout/stderr go to `<cwd>/agent.log` so
/// the agent's chatter doesn't bleed into the test's stderr but is
/// still tail-able while debugging.
async fn spawn_agent(
    children: &DashMap<SandboxId, std::sync::Mutex<Option<tokio::process::Child>>>,
    id: SandboxId,
    agent: &engram_core::types::sandbox::AgentSpec,
    sandbox_env: &HashMap<String, String>,
    cwd: &Path,
) -> Result<(), SandboxError> {
    let argv0 = agent
        .argv
        .first()
        .ok_or_else(|| SandboxError::InvalidSpec("agent argv must not be empty".into()))?;

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cwd.join("agent.log"))
        .map_err(|e| SandboxError::Vm(format!("open agent.log: {e}").into()))?;
    let log_err = log
        .try_clone()
        .map_err(|e| SandboxError::Vm(format!("dup agent.log fd: {e}").into()))?;

    let mut env = sandbox_env.clone();
    env.extend(agent.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    if !env.contains_key("PATH") {
        if let Ok(p) = std::env::var("PATH") {
            env.insert("PATH".into(), p);
        }
    }

    let mut cmd = Command::new(argv0);
    cmd.args(agent.argv.iter().skip(1))
        .current_dir(cwd)
        .env_clear()
        .envs(env_iter(&env))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .kill_on_drop(true);

    let child = cmd
        .spawn()
        .map_err(|e| SandboxError::Vm(format!("spawn agent `{argv0}`: {e}").into()))?;
    children.insert(id, std::sync::Mutex::new(Some(child)));
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    sandbox_id: SandboxId,
    image_version: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

fn env_iter<'a>(env: &'a HashMap<String, String>) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
    env.iter().map(|(k, v)| (k.as_str(), v.as_str()))
}

/// Copy the contents of `src` into the (already-empty) `dst` directory.
///
/// Optimisations the implementation reaches for, in order of preference:
///
/// 1. **macOS APFS clonefile** — via `cp -c -R` which is the supported
///    way to invoke `clonefile(2)` from userland without raw FFI. Both
///    src and dst must be on the same APFS volume (true in practice,
///    since both live under `engram_work_dir`). Materialising a 5 GB
///    starter directory becomes essentially free (metadata-only).
/// 2. **Recursive copy** — fallback for non-macOS, non-APFS, or when
///    the `cp` invocation fails. Plain `tokio::fs::copy` per file.
async fn materialize_rootfs(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // `src/.` makes cp copy *contents* of src into dst rather than
        // creating dst/<basename(src)>. `-c` enables clonefile.
        let src_arg = format!("{}/.", src.display());
        let status = tokio::process::Command::new("cp")
            .arg("-c")
            .arg("-R")
            .arg(src_arg)
            .arg(dst)
            .status()
            .await?;
        if status.success() {
            return Ok(());
        }
        tracing::warn!(
            ?src,
            ?dst,
            "cp -c -R failed (status {:?}); falling back to recursive copy",
            status.code()
        );
    }
    recursive_copy(src, dst).await
}

/// Pure-Rust recursive directory copy. No clonefile / reflink — used
/// as the portable fallback. Symbolic links are followed (a quirk of
/// `tokio::fs::copy`); good enough for typical starter images that
/// don't include intentional symlink trees.
async fn recursive_copy(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((s, d)) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&s).await?;
        tokio::fs::create_dir_all(&d).await?;
        while let Some(entry) = entries.next_entry().await? {
            let from = entry.path();
            let to = d.join(entry.file_name());
            let meta = entry.metadata().await?;
            if meta.is_dir() {
                stack.push((from, to));
            } else if meta.is_file() {
                tokio::fs::copy(&from, &to).await?;
            }
        }
    }
    Ok(())
}

/// Read 8 KiB chunks from a child pipe, wrap each into an `ExecEvent`,
/// and forward into the channel. Returns when the pipe yields EOF, the
/// receiver is dropped, or an I/O error occurs — channel close being
/// the signal that downstream stopped caring.
async fn pipe_to_channel<R>(
    mut reader: R,
    tx: mpsc::Sender<ExecEvent>,
    wrap: fn(Bytes) -> ExecEvent,
) where
    R: AsyncReadExt + Unpin,
{
    let mut buf = vec![0u8; 8 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => return, // EOF
            Ok(n) => {
                let chunk = Bytes::copy_from_slice(&buf[..n]);
                if tx.send(wrap(chunk)).await.is_err() {
                    return; // receiver dropped
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "pipe read error; closing");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn backend() -> (ProcessBackend, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let backend = ProcessBackend::new(dir.path());
        (backend, dir)
    }

    fn spec() -> SandboxSpec {
        SandboxSpec {
            image: "warm-test".into(),
            rootfs_source: None,
            cpu: engram_core::types::sandbox::CpuLimit { vcpus: 1 },
            memory: engram_core::types::sandbox::MemoryLimit { max_mib: 256 },
            disk: engram_core::types::sandbox::DiskLimit { max_gib: 1 },
            ttl: None,
            env: HashMap::new(),
            workdir: None,
            mounts: Vec::new(),
        }
    }

    fn exec(argv: &[&str]) -> ExecRequest {
        ExecRequest {
            command: argv.iter().map(|s| s.to_string()).collect(),
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout: None,
        }
    }

    #[tokio::test]
    async fn create_then_exec_captures_stdout() {
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        let h = b.exec(id, exec(&["sh", "-c", "echo hi"])).await.unwrap();
        assert_eq!(h.exit_status, Some(0));
        assert_eq!(String::from_utf8(h.stdout).unwrap(), "hi\n");
        assert!(h.stderr.is_empty());
    }

    #[tokio::test]
    async fn nonzero_exit_propagates() {
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        let h = b.exec(id, exec(&["sh", "-c", "exit 7"])).await.unwrap();
        assert_eq!(h.exit_status, Some(7));
    }

    #[tokio::test]
    async fn stderr_is_separated_from_stdout() {
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        let h = b
            .exec(id, exec(&["sh", "-c", "echo out; echo err 1>&2"]))
            .await
            .unwrap();
        assert_eq!(String::from_utf8(h.stdout).unwrap(), "out\n");
        assert_eq!(String::from_utf8(h.stderr).unwrap(), "err\n");
    }

    #[tokio::test]
    async fn timeout_kills_runaway_command_and_reports_124() {
        // Streaming exec doesn't error on timeout — it kills the
        // child and surfaces the conventional `timeout(1)` exit
        // code (124) so callers can distinguish "your command exited
        // 124" from "your command timed out" only by also having
        // requested a timeout. Either way the process is dead.
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        let req = ExecRequest {
            command: vec!["sh".into(), "-c".into(), "sleep 30".into()],
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout: Some(Duration::from_millis(100)),
        };
        let started = std::time::Instant::now();
        let h = b.exec(id, req).await.unwrap();
        assert_eq!(h.exit_status, Some(EXIT_CODE_TIMEOUT));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "timeout must actually kill the child, not let it run to completion"
        );
    }

    #[tokio::test]
    async fn stdin_is_forwarded_to_command() {
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        let req = ExecRequest {
            command: vec!["sh".into(), "-c".into(), "cat".into()],
            stdin: Some(b"piped-payload\n".to_vec()),
            env: HashMap::new(),
            workdir: None,
            timeout: Some(Duration::from_secs(2)),
        };
        let h = b.exec(id, req).await.unwrap();
        assert_eq!(h.exit_status, Some(0));
        assert_eq!(String::from_utf8(h.stdout).unwrap(), "piped-payload\n");
    }

    #[tokio::test]
    async fn exec_uses_per_sandbox_cwd() {
        let (b, _d) = backend();
        let id1 = b.create(spec()).await.unwrap();
        let id2 = b.create(spec()).await.unwrap();
        b.exec(id1, exec(&["sh", "-c", "echo s1 > marker"]))
            .await
            .unwrap();
        b.exec(id2, exec(&["sh", "-c", "echo s2 > marker"]))
            .await
            .unwrap();
        let h1 = b.exec(id1, exec(&["cat", "marker"])).await.unwrap();
        let h2 = b.exec(id2, exec(&["cat", "marker"])).await.unwrap();
        assert_eq!(String::from_utf8(h1.stdout).unwrap(), "s1\n");
        assert_eq!(String::from_utf8(h2.stdout).unwrap(), "s2\n");
    }

    #[tokio::test]
    async fn exec_request_env_overrides_spec_env() {
        let (b, _d) = backend();
        let mut s = spec();
        s.env.insert("GREETING".into(), "from-spec".into());
        let id = b.create(s).await.unwrap();
        let mut req_env = HashMap::new();
        req_env.insert("GREETING".into(), "from-request".into());
        let req = ExecRequest {
            command: vec!["sh".into(), "-c".into(), "printf %s \"$GREETING\"".into()],
            stdin: None,
            env: req_env,
            workdir: None,
            timeout: None,
        };
        let h = b.exec(id, req).await.unwrap();
        assert_eq!(String::from_utf8(h.stdout).unwrap(), "from-request");
    }

    #[tokio::test]
    async fn exec_on_unknown_id_is_not_found() {
        let (b, _d) = backend();
        let res = b.exec(SandboxId::new(), exec(&["true"])).await;
        assert!(matches!(res, Err(SandboxError::NotFound)));
    }

    #[tokio::test]
    async fn exec_with_empty_argv_is_invalid_spec() {
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        let req = ExecRequest {
            command: vec![],
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout: None,
        };
        let res = b.exec(id, req).await;
        assert!(matches!(res, Err(SandboxError::InvalidSpec(_))));
    }

    #[tokio::test]
    async fn destroy_removes_cwd_and_state() {
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        b.exec(id, exec(&["sh", "-c", "echo x > f"])).await.unwrap();
        let cwd = b.cwd_for(id);
        assert!(cwd.exists());
        b.destroy(id).await.unwrap();
        assert!(!cwd.exists());
        assert!(matches!(
            b.exec(id, exec(&["true"])).await,
            Err(SandboxError::NotFound)
        ));
    }

    #[tokio::test]
    async fn agent_lifecycle_spawns_at_create_and_dies_at_destroy() {
        // Long-running agent: a sleep loop that writes its PID to
        // a file at startup so the test can probe whether the
        // process is still alive after destroy.
        let (b, _d) = backend();
        let pid_file = "agent.pid";
        let id = b.create(spec()).await.unwrap();
        // Agent argv is supplied at start_agent time, not on the
        // SandboxSpec — see the trait docs for why.
        let agent = AgentSpec {
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!("echo $$ > {pid_file}; exec sleep 60"),
            ],
            env: HashMap::new(),
        };
        b.start_agent(id, agent).await.unwrap();

        // Wait for the agent to write its pid file. Real-world race
        // budgets are tiny here; a few hundred ms is plenty.
        let pid_path = b.cwd_for(id).join(pid_file);
        for _ in 0..50 {
            if pid_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let pid: i32 = fs::read_to_string(&pid_path)
            .expect("agent should have written its pid")
            .trim()
            .parse()
            .expect("pid file should contain an integer");
        // /proc isn't on macOS, but `kill -0` works to probe liveness
        // portably. exit_status==0 ⇔ process exists.
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .unwrap()
            .success();
        assert!(alive, "agent pid {pid} should be alive after create");

        b.destroy(id).await.unwrap();

        // After destroy, the pid should be gone (or, on platforms
        // where the kernel recycles pids quickly, kill -0 may still
        // succeed against an unrelated process — accept that and
        // assert at minimum that the cwd was removed).
        let dead = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| !s.success())
            .unwrap_or(true);
        assert!(dead, "agent pid {pid} should be dead after destroy");
    }

    #[tokio::test]
    async fn destroy_unknown_id_is_idempotent() {
        let (b, _d) = backend();
        b.destroy(SandboxId::new()).await.unwrap();
    }

    #[tokio::test]
    async fn list_reflects_active_sandboxes() {
        let (b, _d) = backend();
        assert!(b.list().await.unwrap().is_empty());
        let a = b.create(spec()).await.unwrap();
        let z = b.create(spec()).await.unwrap();
        let mut listed = b.list().await.unwrap();
        listed.sort();
        let mut expected = vec![a, z];
        expected.sort();
        assert_eq!(listed, expected);
        b.destroy(a).await.unwrap();
        let after: Vec<_> = b.list().await.unwrap();
        assert_eq!(after, vec![z]);
    }

    #[tokio::test]
    async fn snapshot_then_restore_round_trips_workspace() {
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        b.exec(
            id,
            exec(&[
                "sh",
                "-c",
                "echo hello > greeting && mkdir sub && echo nested > sub/file",
            ]),
        )
        .await
        .unwrap();

        let snap_dir = tempfile::tempdir().unwrap();
        let meta = b.snapshot(id, snap_dir.path()).await.unwrap();
        assert!(meta.size_bytes > 0);
        assert_eq!(meta.image_version, "warm-test");
        assert!(snap_dir.path().join("manifest.json").is_file());
        assert!(snap_dir.path().join("fs.tar.gz").is_file());

        // Destroy the original so any restore reads from the snapshot,
        // not from leftover state.
        b.destroy(id).await.unwrap();

        let restored = b.restore(snap_dir.path().to_path_buf()).await.unwrap();
        let h_top = b.exec(restored, exec(&["cat", "greeting"])).await.unwrap();
        assert_eq!(String::from_utf8(h_top.stdout).unwrap(), "hello\n");
        let h_nested = b.exec(restored, exec(&["cat", "sub/file"])).await.unwrap();
        assert_eq!(String::from_utf8(h_nested.stdout).unwrap(), "nested\n");
    }

    #[tokio::test]
    async fn snapshot_unknown_id_is_not_found() {
        let (b, _d) = backend();
        let snap_dir = tempfile::tempdir().unwrap();
        let res = b.snapshot(SandboxId::new(), snap_dir.path()).await;
        assert!(matches!(res, Err(SandboxError::NotFound)));
    }

    #[tokio::test]
    async fn restore_with_corrupted_manifest_errors_cleanly() {
        let (b, _d) = backend();
        let snap_dir = tempfile::tempdir().unwrap();
        fs::write(snap_dir.path().join("manifest.json"), b"not json").unwrap();
        fs::write(snap_dir.path().join("fs.tar.gz"), b"").unwrap();
        let res = b.restore(snap_dir.path().to_path_buf()).await;
        assert!(matches!(res, Err(SandboxError::Snapshot(_))));
    }

    #[tokio::test]
    async fn exec_stream_yields_chunks_before_process_exits() {
        // Streaming contract: stdout chunks arrive *while* the child is
        // still running, not all at once at the end. Run a script that
        // prints, sleeps, prints, sleeps, exits — and verify the first
        // chunk shows up well before the child would have finished.
        use engram_core::types::ExecEvent;
        use futures::StreamExt;

        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        let stream = b
            .exec_stream(
                id,
                exec(&[
                    "sh",
                    "-c",
                    // Two writes separated by sleep. Force a flush
                    // after each printf or `sh` will line-buffer.
                    "printf chunk-1; sleep 0.5; printf chunk-2",
                ]),
            )
            .await
            .unwrap();

        let started = std::time::Instant::now();
        let mut events = stream.events;
        let mut first_stdout_at: Option<Duration> = None;
        let mut all_stdout = Vec::new();
        let mut exit = None;
        while let Some(ev) = events.next().await {
            match ev {
                ExecEvent::Stdout(b) => {
                    if first_stdout_at.is_none() {
                        first_stdout_at = Some(started.elapsed());
                    }
                    all_stdout.extend_from_slice(&b);
                }
                ExecEvent::Stderr(_) => {}
                ExecEvent::Exit(code) => {
                    exit = code;
                    break;
                }
            }
        }
        assert_eq!(exit, Some(0));
        assert_eq!(String::from_utf8(all_stdout).unwrap(), "chunk-1chunk-2");

        let first = first_stdout_at.expect("at least one stdout chunk arrived");
        // Child sleeps 500ms between writes. First chunk must arrive
        // well before the child finishes (1s budget after process spawn).
        // If we'd buffered to completion this would be ≥500ms.
        assert!(
            first < Duration::from_millis(450),
            "first chunk took {first:?} — streaming would expect it sooner",
        );
    }

    #[tokio::test]
    async fn create_with_rootfs_source_materializes_into_cwd() {
        // Build a starter image directory, create a sandbox pointing
        // at it, and verify the files appear in the sandbox cwd.
        // Uses APFS clonefile via `cp -c -R` on macOS (essentially
        // free) and a recursive copy elsewhere — both produce the
        // same observable result.
        let img = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(img.path().join("scripts")).unwrap();
        std::fs::write(img.path().join("README.md"), b"# starter\n").unwrap();
        std::fs::write(img.path().join("scripts/setup.sh"), b"echo hi\n").unwrap();

        let (b, _d) = backend();
        let mut s = spec();
        s.rootfs_source = Some(img.path().to_path_buf());
        let id = b.create(s).await.unwrap();

        // Verify visibility from inside the sandbox via exec.
        let h = b
            .exec(id, exec(&["sh", "-c", "ls scripts && cat README.md"]))
            .await
            .unwrap();
        assert_eq!(h.exit_status, Some(0));
        let out = String::from_utf8(h.stdout).unwrap();
        assert!(out.contains("setup.sh"));
        assert!(out.contains("# starter"));
    }

    #[tokio::test]
    async fn create_without_rootfs_source_leaves_empty_cwd() {
        // Backwards-compatible default: no rootfs_source means the
        // sandbox cwd is just an empty directory. Older callers /
        // tests don't have to know about images.
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        let h = b
            .exec(id, exec(&["sh", "-c", "ls -A | wc -l"]))
            .await
            .unwrap();
        let count = String::from_utf8(h.stdout)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert_eq!(count, 0, "empty cwd when no rootfs_source");
    }

    #[tokio::test]
    async fn create_with_missing_rootfs_source_errors() {
        let (b, _d) = backend();
        let mut s = spec();
        s.rootfs_source = Some("/no/such/starter/dir".into());
        match b.create(s).await {
            Err(SandboxError::Vm(e)) => {
                assert!(e.to_string().contains("materialize"));
            }
            other => panic!("expected materialize error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn workdir_is_resolved_relative_to_sandbox_cwd() {
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        b.exec(id, exec(&["mkdir", "-p", "deep/nested"]))
            .await
            .unwrap();
        let req = ExecRequest {
            command: vec!["pwd".into()],
            stdin: None,
            env: HashMap::new(),
            workdir: Some("deep/nested".into()),
            timeout: None,
        };
        let h = b.exec(id, req).await.unwrap();
        let pwd = String::from_utf8(h.stdout).unwrap();
        assert!(
            pwd.trim().ends_with("deep/nested"),
            "pwd = {pwd:?} should end at the requested workdir",
        );
    }
}
