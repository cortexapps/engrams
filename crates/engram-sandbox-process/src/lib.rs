//! Local-dev `SandboxBackend` that runs commands as plain host
//! subprocesses, with each "sandbox" rooted at its own working
//! directory under `work_dir`.
//!
//! # Safety / scope
//!
//! **There is no isolation here.** This backend exists so the entire
//! orchestration layer (coordinator API, scheduler, snapshot manager,
//! blob storage, host-agent loops) can be exercised on a developer's
//! laptop — including macOS Apple Silicon, where the production VMM
//! (Firecracker, KVM-only) cannot run. Real isolation comes from
//! `engram-sandbox-firecracker` on Linux production hosts.
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
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use dashmap::{mapref::entry::Entry, DashMap};
use engram_core::traits::sandbox::SandboxBackend;
use engram_core::types::endpoints::GuestEndpoints;
use engram_core::types::ids::{SandboxId, SnapshotId};
use engram_core::types::sandbox::{
    AgentSpec, ExecEvent, ExecRequest, ExecStream, SandboxSpec, WriteFileResult, WriteFileSpec,
};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::SandboxError;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;

/// POSIX `timeout(1)` convention: command was killed because it
/// exceeded its wall-clock budget. Surfaced as the exit status of an
/// `ExecEvent::Exit` in this case so callers can distinguish a slow
/// command from a normal nonzero exit.
const EXIT_CODE_TIMEOUT: i32 = 124;

/// Match the guest journal's per-stream retention bound. Live consumers
/// continue receiving bytes beyond this point, but later attaches can only
/// replay the retained prefix.
const EXEC_OUTPUT_CAP: usize = 64 * 1024 * 1024;

/// A few MiB of live output at the 8 KiB pipe-read size. Slow consumers can
/// recover a lag from the retained buffers until the output cap is crossed.
const EXEC_LIVE_EVENT_CAPACITY: usize = 512;

/// Completed exec records retained per sandbox for late re-attach (the
/// in-memory analogue of the guest journal's 32-record budget). Without a
/// bound, a long-lived dev sandbox running many execs accumulates output
/// buffers indefinitely. Running records are never evicted — they are
/// bounded by actual concurrency. Attaching to an evicted ticket behaves
/// like a TTL'd guest journal: nonzero offsets refuse loudly, zero offsets
/// are a fresh attach-or-start.
const EXEC_COMPLETED_RETENTION: usize = 32;

/// Monotonic completion order across all records; drives oldest-first
/// eviction above [`EXEC_COMPLETED_RETENTION`].
static EXEC_COMPLETION_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Per-sandbox state owned by the backend.
#[derive(Clone, Debug)]
struct SandboxState {
    spec: SandboxSpec,
    cwd: PathBuf,
    /// ADR 0103: process sandboxes themselves do not survive a host-agent
    /// restart, so their durable-exec equivalent lives in memory for exactly
    /// the sandbox lifetime. An on-disk `ExecJournal` would buy nothing:
    /// after restart the sandbox lookup is already `NotFound`.
    exec_records: Arc<DashMap<String, Arc<ExecRecord>>>,
}

impl SandboxState {
    fn new(spec: SandboxSpec, cwd: PathBuf) -> Self {
        Self {
            spec,
            cwd,
            exec_records: Arc::new(DashMap::new()),
        }
    }
}

#[derive(Debug)]
struct ExecRecord {
    command: Vec<String>,
    pgid: Option<u32>,
    state: Mutex<ExecRecordState>,
    progress: broadcast::Sender<ExecRecordEvent>,
}

#[derive(Debug, Default)]
struct ExecRecordState {
    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
    stdout_len: u64,
    stderr_len: u64,
    stdout_truncated: bool,
    stderr_truncated: bool,
    /// `None` means running; `Some(None)` is an exact signal/unknown exit.
    exit: Option<Option<i32>>,
    /// Completion order for retention eviction; `None` while running.
    completed_seq: Option<u64>,
}

#[derive(Clone, Debug)]
enum ExecRecordEvent {
    Stdout { offset: u64, bytes: Bytes },
    Stderr { offset: u64, bytes: Bytes },
    Exit(Option<i32>),
}

impl ExecRecord {
    fn new(command: Vec<String>, pgid: Option<u32>) -> Self {
        let (progress, _) = broadcast::channel(EXEC_LIVE_EVENT_CAPACITY);
        Self {
            command,
            pgid,
            state: Mutex::new(ExecRecordState::default()),
            progress,
        }
    }

    fn append_stdout(&self, bytes: Bytes) {
        let mut state = self.state.lock();
        let state = &mut *state;
        let offset = state.stdout_len;
        state.stdout_len = state.stdout_len.saturating_add(bytes.len() as u64);
        append_retained(&mut state.stdout_buf, &mut state.stdout_truncated, &bytes);
        // Send under the state lock so an attacher that subscribes before
        // taking its replay snapshot cannot observe new total length without
        // the corresponding live event already being queued.
        let _ = self
            .progress
            .send(ExecRecordEvent::Stdout { offset, bytes });
    }

    fn append_stderr(&self, bytes: Bytes) {
        let mut state = self.state.lock();
        let state = &mut *state;
        let offset = state.stderr_len;
        state.stderr_len = state.stderr_len.saturating_add(bytes.len() as u64);
        append_retained(&mut state.stderr_buf, &mut state.stderr_truncated, &bytes);
        let _ = self
            .progress
            .send(ExecRecordEvent::Stderr { offset, bytes });
    }

    fn finish(&self, exit: Option<i32>) {
        let mut state = self.state.lock();
        state.exit = Some(exit);
        state.completed_seq =
            Some(EXEC_COMPLETION_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let _ = self.progress.send(ExecRecordEvent::Exit(exit));
    }
}

/// Evict the oldest completed records beyond [`EXEC_COMPLETED_RETENTION`].
/// Called before each new exec's entry lookup — never while holding a map
/// entry guard (iteration and entry() both take shard locks).
fn prune_completed_exec_records(
    records: &DashMap<String, Arc<ExecRecord>>,
    requested_exec_id: &str,
) {
    let mut completed: Vec<(String, u64)> = records
        .iter()
        .filter_map(|entry| {
            if entry.key() == requested_exec_id {
                return None;
            }
            entry
                .value()
                .state
                .lock()
                .completed_seq
                .map(|seq| (entry.key().clone(), seq))
        })
        .collect();
    if completed.len() <= EXEC_COMPLETED_RETENTION {
        return;
    }
    completed.sort_by_key(|(_, seq)| *seq);
    for (exec_id, _) in completed
        .iter()
        .take(completed.len() - EXEC_COMPLETED_RETENTION)
    {
        records.remove(exec_id);
    }
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

    fn cwd_for(&self, id: SandboxId) -> PathBuf {
        self.work_dir.join(id.to_string())
    }

    /// ADR 0007 Phase 6: per-snapshot staging dir, owned by the
    /// backend (coord no longer dictates layout). Lives next to
    /// `cwd_for` sandboxes but under a `snapshots/` subtree so
    /// the destroy path's `remove_dir_all(cwd)` doesn't sweep
    /// snapshots.
    fn snapshot_dir_for(&self, snapshot_id: engram_core::types::SnapshotId) -> PathBuf {
        self.work_dir
            .join("snapshots")
            .join(snapshot_id.to_string())
    }
}

#[async_trait]
impl SandboxBackend for ProcessBackend {
    fn harness_dial(&self) -> engram_core::traits::HarnessDial {
        // Harness exec'd as a host subprocess; dials TCP loopback
        // back to the coord-side harness listener.
        engram_core::traits::HarnessDial::HostTcp
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
        // ADR 0027 dev parity: ProcessBackend has no virtio-blk drives, so
        // it symlinks each bundle's guest mount (under the materialized cwd)
        // at a host-local unpacked bundle dir. agentd's activation step (run
        // in `start_agent` below) then wires skills from these the same
        // way the FC guest does.
        stage_aux_bundles(&cwd).await;
        self.sandboxes.insert(id, SandboxState::new(spec, cwd));
        Ok(id)
    }

    async fn merge_session_env(
        &self,
        id: SandboxId,
        env: HashMap<String, String>,
    ) -> Result<(), SandboxError> {
        // ADR 0020: a restored base snapshot's spec carries only the
        // generic image env; merge the per-session env (manifest env +
        // secrets + session id) so `exec_stream` (which reads
        // `state.spec.env`) sees it, matching the cold-create contract.
        let mut state = self.sandboxes.get_mut(&id).ok_or(SandboxError::NotFound)?;
        state.spec.env.extend(env);
        Ok(())
    }

    async fn start_agent(&self, id: SandboxId, agent: AgentSpec) -> Result<(), SandboxError> {
        let state = self
            .sandboxes
            .get(&id)
            .ok_or(SandboxError::NotFound)?
            .clone();
        // ADR 0027: wire the staged RO bundles into the harness discovery
        // paths under the sandbox cwd — the dev mirror of agentd's
        // SpawnHarness activation. `root = cwd`; spawn_agent sets
        // HOME=<cwd>/root so the harness resolves ~/.claude/skills there.
        // Run BEFORE the readiness-probe early return (like agentd) so a
        // dev_vm session's /exec + shell also see the skills. Gate on the
        // union of session_env + the per-spawn agent.env, because the forge
        // broker token rides agent.env, not session_env.
        let mut gate_env = state.spec.env.clone();
        gate_env.extend(agent.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        let report = engram_session_bundles::activate(&state.cwd, &gate_env);
        if !report.activated.is_empty() {
            tracing::info!(activated = ?report.activated, "ADR 0027: activated session bundles (dev)");
        }
        for w in &report.warnings {
            tracing::debug!(warning = %w, "ADR 0027: session bundle activation (dev)");
        }
        // ADR 0015 M1: empty argv is a readiness probe (no harness
        // to spawn). Mirrors `engram_agentd::HarnessSupervisor::spawn`.
        // Coord's harness=none cold-create path calls us with empty
        // argv to confirm the sandbox is reachable, without launching
        // anything.
        if agent.argv.is_empty() {
            return Ok(());
        }
        // Idempotent: a second call with the agent already running
        // is a no-op. Resumed-then-checkpointed-then-resumed flows may
        // call start_agent more than once; we keep the first agent.
        if self.agent_children.contains_key(&id) {
            return Ok(());
        }
        // ADR 0067: stamp the attach token into the harness child env —
        // the backend is the only party that knows the sandbox id
        // pre-boot; the epoch was minted coordinator-side into the spec.
        let token_env = agent.attach_token_env(id);
        let mut agent = agent;
        agent.env.extend(token_env);
        spawn_agent(
            &self.agent_children,
            id,
            &agent,
            &state.spec.env,
            &state.cwd,
        )
        .await
    }

    /// ADR 0066: Process has no VM boundary — agentd runs as a host subprocess,
    /// so a preview's target port is already on the host's `127.0.0.1`. Return
    /// `None` so the host-agent dials the loopback port directly (no vsock
    /// relay, none exists here). Explicit for discoverability; matches the
    /// trait default.
    async fn open_guest_stream(
        &self,
        _id: SandboxId,
        _port: u32,
    ) -> Result<Option<engram_core::traits::sandbox::HarnessByteStream>, SandboxError> {
        Ok(None)
    }

    async fn exec_stream(
        &self,
        id: SandboxId,
        req: ExecRequest,
    ) -> Result<ExecStream, SandboxError> {
        let caller_supplied_exec_id = req.exec_id.is_some();
        let exec_id = req
            .exec_id
            .clone()
            .unwrap_or_else(|| SandboxId::new().to_string());
        let state = self
            .sandboxes
            .get(&id)
            .ok_or(SandboxError::NotFound)?
            .clone();

        let exec_records = state.exec_records.clone();
        prune_completed_exec_records(&exec_records, &exec_id);
        let entry = exec_records.entry(exec_id.clone());
        match entry {
            Entry::Occupied(existing) => {
                let record = existing.get().clone();
                drop(existing);
                return Ok(attach_exec_record(
                    id,
                    exec_id,
                    record,
                    &req.command,
                    req.stdout_offset.unwrap_or(0),
                    req.stderr_offset.unwrap_or(0),
                ));
            }
            Entry::Vacant(vacant) => {
                let stdout_offset = req.stdout_offset.unwrap_or(0);
                let stderr_offset = req.stderr_offset.unwrap_or(0);
                if caller_supplied_exec_id && (stdout_offset != 0 || stderr_offset != 0) {
                    return Ok(refused_exec_stream(
                        id,
                        exec_id.clone(),
                        format!(
                            "durable exec reattach failed: record for exec_id {exec_id} is missing; \
                             refusing to spawn a second command with requested offsets \
                             stdout={stdout_offset}, stderr={stderr_offset}\n"
                        ),
                    ));
                }

                let argv =
                    req.command.first().cloned().ok_or_else(|| {
                        SandboxError::InvalidSpec("argv must not be empty".into())
                    })?;
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
                    .kill_on_drop(true)
                    // Put the child in its own process group so a timeout kill
                    // can take the whole tree down with a single
                    // `kill(-pgid, SIGKILL)`. Without this, commands like
                    // `sh -c "sleep 30"` on shells that *fork* (Ubuntu's dash
                    // for `-c`, vs bash which execs) leak the inner sleep —
                    // killing the shell leaves the child orphaned, holding
                    // stdout/stderr pipes open, blocking the drain task for
                    // the full natural duration. (Verified failure mode on
                    // Blacksmith Ubuntu runners; macOS+bash and the dev VM's
                    // nix-bash exec the inner command, so the bug was hidden.)
                    .process_group(0);

                // The vacant ticket guard stays held through spawn, making
                // first-writer-wins atomic: no concurrent caller can observe
                // the same ticket as absent and spawn a second process.
                let mut child = cmd
                    .spawn()
                    .map_err(|e| SandboxError::Vm(format!("spawn `{argv}`: {e}").into()))?;
                let stdin = child.stdin.take();
                let stdout = child
                    .stdout
                    .take()
                    .ok_or_else(|| SandboxError::Vm("stdout pipe missing".into()))?;
                let stderr = child
                    .stderr
                    .take()
                    .ok_or_else(|| SandboxError::Vm("stderr pipe missing".into()))?;
                let record = Arc::new(ExecRecord::new(req.command.clone(), child.id()));
                vacant.insert(record.clone());

                // Subscribe before starting either pipe drain. This makes the
                // first stream gapless even if the command fills a pipe as
                // soon as it is spawned.
                let stream =
                    attach_exec_record(id, exec_id.clone(), record.clone(), &req.command, 0, 0);
                spawn_exec_record_owner(
                    exec_id,
                    record,
                    SpawnedExec {
                        child,
                        stdin,
                        input: req.stdin,
                        stdout,
                        stderr,
                        timeout: req
                            .timeout
                            .or(state.spec.ttl)
                            .unwrap_or(Duration::from_secs(60 * 60)),
                    },
                );
                return Ok(stream);
            }
        }
    }

    async fn cancel_exec(&self, id: SandboxId, exec_id: String) -> Result<(), SandboxError> {
        let state = self
            .sandboxes
            .get(&id)
            .ok_or(SandboxError::NotFound)?
            .clone();
        let record = state
            .exec_records
            .get(&exec_id)
            .map(|entry| entry.clone())
            .ok_or(SandboxError::NotFound)?;
        if record.state.lock().exit.is_some() {
            return Ok(());
        }
        kill_exec_process_group(record.pgid, &exec_id)
    }

    async fn write_files(
        &self,
        id: SandboxId,
        files: Vec<WriteFileSpec>,
    ) -> Result<Vec<WriteFileResult>, SandboxError> {
        let state = self
            .sandboxes
            .get(&id)
            .ok_or(SandboxError::NotFound)?
            .clone();
        let mut results = Vec::with_capacity(files.len());
        for file in files {
            // Match exec's workdir resolution: relative paths are rooted in
            // the per-sandbox cwd; absolute paths remain absolute.
            let resolved = state.cwd.join(&file.path);
            let outcome: std::io::Result<()> = async {
                if let Some(parent) = resolved.parent() {
                    if !parent.as_os_str().is_empty() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                }
                tokio::fs::write(&resolved, &file.content).await?;
                if let Some(mode) = file.mode {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        tokio::fs::set_permissions(
                            &resolved,
                            std::fs::Permissions::from_mode(mode),
                        )
                        .await?;
                    }
                }
                Ok(())
            }
            .await;
            results.push(match outcome {
                Ok(()) => WriteFileResult {
                    path: file.path,
                    ok: true,
                    error: None,
                },
                Err(error) => WriteFileResult {
                    path: file.path,
                    ok: false,
                    error: Some(error.to_string()),
                },
            });
        }
        Ok(results)
    }

    async fn snapshot(&self, id: SandboxId) -> Result<SnapshotMetadata, SandboxError> {
        let state = self
            .sandboxes
            .get(&id)
            .ok_or(SandboxError::NotFound)?
            .clone();

        // ADR 0007 Phase 6: backend chooses its own staging dir.
        // Allocate the snapshot_id first so we know the path
        // before writing anything.
        let snapshot_id = SnapshotId::new();
        let dest = self.snapshot_dir_for(snapshot_id);
        tokio::fs::create_dir_all(&dest).await?;

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
            id: snapshot_id,
            size_bytes: manifest_size + archive_size,
            created_at: manifest.created_at,
            image_version: manifest.image_version,
            // ProcessBackend doesn't write disks in ext4 form; chunked
            // storage doesn't apply here (the rootfs is a directory).
            disk_manifest: None,
            // No memory snapshot in ProcessBackend — there's no
            // guest RAM to capture.
            memory_manifest: None,
            base_memory_manifest: None,
            migration_source: None,
            // ADR 0014: ProcessBackend is dev/test only; no portable
            // BlobStorage upload.
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
            aux_bundles: vec![],
            // Issue #529: no real pause instant when run unwrapped by
            // `PooledBackend` (which stamps it from its own capture_phase);
            // dev-only backend, so the composed path's `now` fallback is fine.
            paused_at: None,
            // ADR 0095: capture never stamps peer hints; the resume
            // assembler does, coordinator-side.
            peer_hints: Vec::new(),
        })
    }

    fn snapshot_path_for(&self, snapshot_id: SnapshotId) -> PathBuf {
        self.snapshot_dir_for(snapshot_id)
    }

    /// `restore` returns a *new* SandboxId pointing at a freshly-untarred
    /// cwd. This re-creates a SandboxSpec; we don't carry resource
    /// limits or rootfs_source through a snapshot because:
    ///   - Resource limits aren't enforced by ProcessBackend anyway
    ///     (set them at the next exec via the request).
    ///   - `rootfs_source` is irrelevant after restore — the cwd is
    ///     materialized from the snapshot's tarball, not from a source
    ///     image directory.
    async fn restore(&self, metadata: SnapshotMetadata) -> Result<SandboxId, SandboxError> {
        // ADR 0007 Phase 6: backend looks up its own staging dir.
        let src = self.snapshot_dir_for(metadata.id);
        let manifest_bytes = match tokio::fs::read(src.join("manifest.json")).await {
            Ok(b) => b,
            // ADR 0020: a base (template) snapshot carries no captured
            // process state — this backend's "process memory" isn't
            // snapshotted, only a session capture writes fs.tar.gz +
            // manifest.json. Restoring a base snapshot therefore means
            // "boot a fresh sandbox for this image" (the FC backend
            // materializes real artifacts from BlobStorage; ProcessBackend
            // has none to materialize). A *corrupted* manifest still
            // errors below — only a genuinely-absent one takes this path.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let id = SandboxId::new();
                let cwd = self.cwd_for(id);
                tokio::fs::create_dir_all(&cwd).await?;
                let spec = SandboxSpec {
                    image: metadata.image_version.clone(),
                    rootfs_source: None,
                    image_uri: None,
                    rootfs_manifest: None,
                    cpu: engram_core::types::sandbox::CpuLimit { vcpus: 1 },
                    memory: engram_core::types::sandbox::MemoryLimit { max_mib: 0 },
                    disk: engram_core::types::sandbox::DiskLimit { max_gib: 0 },
                    ttl: None,
                    env: HashMap::new(),
                    workdir: None,
                    network: Default::default(),
                    aux_ro_drives: Vec::new(),
                };
                self.sandboxes.insert(id, SandboxState::new(spec, cwd));
                return Ok(id);
            }
            Err(e) => return Err(e.into()),
        };
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

        // ADR 0027 dev parity: re-stage the bundle symlinks under the NEW
        // cwd. The untarred tree may carry symlinks pointing at the old
        // cwd (now stale); restaging repoints them at the current host
        // bundle dirs so the resumed harness's skills resolve.
        stage_aux_bundles(&cwd).await;

        // Synthesize a SandboxSpec from the manifest. We don't carry
        // CPU/memory/etc. through the snapshot — the next launch is a
        // fresh process, so those don't apply. `rootfs_source` is also
        // dropped: the cwd was just rehydrated from the snapshot's
        // tarball, not from a source image directory.
        let spec = SandboxSpec {
            image: manifest.image_version,
            rootfs_source: None,
            image_uri: None,
            rootfs_manifest: None,
            cpu: engram_core::types::sandbox::CpuLimit { vcpus: 1 },
            memory: engram_core::types::sandbox::MemoryLimit { max_mib: 0 },
            disk: engram_core::types::sandbox::DiskLimit { max_gib: 0 },
            ttl: None,
            env: HashMap::new(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: Vec::new(),
        };
        self.sandboxes.insert(id, SandboxState::new(spec, cwd));
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
            // Exec lifecycle tasks are detached from consumer streams, so
            // sandbox destruction explicitly kills every still-running
            // record before dropping the sandbox-owned ticket map.
            for entry in state.exec_records.iter() {
                let record = entry.value();
                if record.state.lock().exit.is_none() {
                    let _ = kill_exec_process_group(record.pgid, entry.key());
                }
            }
            state.exec_records.clear();
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
    /// anything ttyd binds is reachable on localhost, and there's no
    /// netns indirection, so `egress_identity` and `dial_ip` are both
    /// loopback. Doesn't bother confirming the sandbox is alive; the
    /// caller deals with the connection failure if it isn't.
    async fn guest_endpoints(&self, id: SandboxId) -> Option<GuestEndpoints> {
        if self.sandboxes.contains_key(&id) {
            let loopback = std::net::Ipv4Addr::LOCALHOST;
            Some(GuestEndpoints {
                egress_identity: loopback,
                dial_ip: loopback,
                netns: None,
                vsock_uds: None,
            })
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
    // ADR 0027 dev parity: mirror the FC guest so the harness finds the
    // bundle-activated skills. In the guest HOME=/root and the skill
    // wrappers live on /usr/local/bin; here both are rooted under the
    // sandbox cwd. Default HOME (don't clobber an explicit one) and
    // prepend the cwd-local bin dir so `engram-share`/`git-askpass` resolve.
    env.entry("HOME".into())
        .or_insert_with(|| cwd.join("root").to_string_lossy().into_owned());
    // A harness derives its whole per-session state (sockets, generated agent
    // config, resume stamps) from this root, whose default is the fixed in-VM
    // path. This backend has no guest, so every sandbox on the host — and any
    // engrams session the dev stack itself runs inside — would otherwise share
    // that ONE directory: sandbox B unlinks and rebinds sandbox A's live hook
    // socket. Root it in the sandbox dir, which is also what snapshot/restore
    // carries.
    // (`ENGRAM_STATE_DIR` is the harness-side contract —
    // `engram_harness_sdk::state`; named here as a literal, like every other
    // guest env this backend fills in.)
    env.entry("ENGRAM_STATE_DIR".into())
        .or_insert_with(|| cwd.join(".engrams").to_string_lossy().into_owned());
    let local_bin = cwd.join("usr/local/bin");
    let local_bin = local_bin.to_string_lossy();
    match env.get("PATH") {
        Some(p) if !p.split(':').any(|seg| seg == local_bin) => {
            env.insert("PATH".into(), format!("{local_bin}:{p}"));
        }
        None => {
            env.insert("PATH".into(), local_bin.into_owned());
        }
        _ => {}
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

/// ADR 0027 dev parity. Symlink each bundle's guest mount (relative to the
/// materialized `cwd`) at a host-local unpacked bundle dir, so
/// `engram_session_bundles::activate(cwd, ..)` finds the bundle there.
/// `just bundles` populates the defaults; override per drive via
/// `ENGRAM_<DRIVE_ID>_BUNDLE_DIR`.
///
/// ADR 0055 dev parity for the built-in skill bundles. Production FC reserves
/// `dyn-*` slots and `patch_drive`s the selected skills in; the non-isolated
/// dev backend has no drives, so it symlinks each available bundle tree (under
/// `var/bundles/<name>`, populated by `just bundles`, or the
/// `ENGRAM_<NAME>_BUNDLE_DIR` override) at its guest mount so
/// `engram_session_bundles::activate(cwd, ..)` finds it. Spec-independent (the
/// restore path synthesizes a spec without aux drives) so create and restore
/// stage identically. The catalog-driven, per-session-selected dev staging
/// lands with the rest of the ADR 0055 catalog; today it stages the known
/// built-in bundles at their canonical guest mounts.
async fn stage_aux_bundles(cwd: &Path) {
    const DEV_BUNDLES: &[&str] = &["skills", "browser", "integrations-cli"];
    // Sequential slot index, mirroring the production init-shim's
    // /opt/engram/dyn/<i> mounting so the shared `activate()` finds the bundles.
    // A skipped (absent) bundle doesn't consume an index.
    let mut i = 0usize;
    for name in DEV_BUNDLES {
        let Some(host_dir) = bundle_host_dir(name) else {
            continue;
        };
        if !host_dir.exists() {
            tracing::debug!(
                bundle = %name,
                host_dir = %host_dir.display(),
                "ADR 0055 dev: bundle dir absent; skipping (run `just bundles`)",
            );
            continue;
        }
        let link = cwd.join(format!("opt/engram/dyn/{i}"));
        if let Some(parent) = link.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let _ = tokio::fs::remove_file(&link).await; // replace stale symlink
        if let Err(e) = tokio::fs::symlink(&host_dir, &link).await {
            tracing::debug!(bundle = %name, error = %e, "ADR 0055 dev: bundle symlink failed");
        }
        i += 1;
    }
}

/// Resolve the host-local unpacked bundle dir for a dev aux drive:
/// `ENGRAM_<DRIVE_ID>_BUNDLE_DIR` if set, else `var/bundles/<drive_id>`
/// relative to the process cwd (the repo root under `just dev`).
fn bundle_host_dir(drive_id: &str) -> Option<PathBuf> {
    let key = format!("ENGRAM_{}_BUNDLE_DIR", drive_id.to_uppercase());
    if let Ok(p) = std::env::var(&key) {
        return Some(PathBuf::from(p));
    }
    Some(PathBuf::from("var/bundles").join(drive_id))
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

fn append_retained(buffer: &mut Vec<u8>, truncated: &mut bool, bytes: &[u8]) {
    let remaining = EXEC_OUTPUT_CAP.saturating_sub(buffer.len());
    let keep = remaining.min(bytes.len());
    buffer.extend_from_slice(&bytes[..keep]);
    if keep < bytes.len() {
        *truncated = true;
    }
}

struct SpawnedExec {
    child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    input: Option<Vec<u8>>,
    stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    timeout: Duration,
}

fn spawn_exec_record_owner(exec_id: String, record: Arc<ExecRecord>, spawned: SpawnedExec) {
    let SpawnedExec {
        mut child,
        stdin,
        input,
        stdout,
        stderr,
        timeout,
    } = spawned;
    // The lifecycle task, rather than any returned stream, owns the child and
    // both drains. Dropping every consumer only drops broadcast receivers; the
    // command continues and completes its sandbox-owned record.
    let stdout_record = record.clone();
    let stdout_handle = tokio::spawn(async move {
        pipe_to_record(stdout, stdout_record, ExecOutput::Stdout).await;
    });
    let stderr_record = record.clone();
    let stderr_handle = tokio::spawn(async move {
        pipe_to_record(stderr, stderr_record, ExecOutput::Stderr).await;
    });
    if let (Some(mut sink), Some(input)) = (stdin, input) {
        tokio::spawn(async move {
            if let Err(error) = sink.write_all(&input).await {
                tracing::debug!(%error, "exec stdin write ended early");
            }
        });
    }

    tokio::spawn(async move {
        let exit = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(Ok(status)) => status.code(),
            Ok(Err(error)) => {
                tracing::warn!(%error, %exec_id, "child wait failed");
                None
            }
            Err(_) => {
                // Timeout remains a two-stage kill:
                //   1. SIGKILL the process group so grandchildren die too.
                //   2. SIGKILL/reap the leader through tokio's Child handle.
                //
                // The group exists because `.process_group(0)` made the child
                // its leader at spawn time.
                let pid = child.id();
                if let Err(error) = kill_exec_process_group(pid, &exec_id) {
                    tracing::warn!(%error, %exec_id, ?pid, "process-group timeout kill failed");
                }
                if let Err(error) = child.start_kill() {
                    tracing::warn!(%error, %exec_id, ?pid, "child.start_kill() after timeout failed");
                }
                if let Err(error) = child.wait().await {
                    tracing::warn!(%error, %exec_id, ?pid, "child.wait() after timeout kill failed");
                }
                Some(EXIT_CODE_TIMEOUT)
            }
        };

        // Both pipes reach EOF before the terminal record is published, so
        // every consumer observes all available output before Exit.
        let _ = stdout_handle.await;
        let _ = stderr_handle.await;
        record.finish(exit);
    });
}

#[derive(Clone, Copy)]
enum ExecOutput {
    Stdout,
    Stderr,
}

async fn pipe_to_record<R>(mut reader: R, record: Arc<ExecRecord>, output: ExecOutput)
where
    R: AsyncReadExt + Unpin,
{
    let mut buf = vec![0u8; 8 * 1024];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => return, // EOF
            Ok(n) => {
                let chunk = Bytes::copy_from_slice(&buf[..n]);
                match output {
                    ExecOutput::Stdout => record.append_stdout(chunk),
                    ExecOutput::Stderr => record.append_stderr(chunk),
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "pipe read error; closing");
                return;
            }
        }
    }
}

fn attach_exec_record(
    sandbox_id: SandboxId,
    exec_id: String,
    record: Arc<ExecRecord>,
    command: &[String],
    stdout_offset: u64,
    stderr_offset: u64,
) -> ExecStream {
    if record.command != command {
        return refused_exec_stream(
            sandbox_id,
            exec_id.clone(),
            format!(
                "exec_id {exec_id} already belongs to command {:?}; refusing different command \
                 {:?} (first writer wins)\n",
                record.command, command
            ),
        );
    }

    // Subscribe before taking the state snapshot. Record publishers send
    // under the same state lock, so bytes produced after subscription are
    // represented either in this snapshot, in the live receiver, or both.
    let mut progress = record.progress.subscribe();
    let snapshot = {
        let state = record.state.lock();
        if stdout_offset > state.stdout_buf.len() as u64 {
            return refused_offset_stream(
                sandbox_id,
                exec_id,
                "stdout",
                stdout_offset,
                state.stdout_buf.len() as u64,
                state.stdout_truncated,
            );
        }
        if stderr_offset > state.stderr_buf.len() as u64 {
            return refused_offset_stream(
                sandbox_id,
                exec_id,
                "stderr",
                stderr_offset,
                state.stderr_buf.len() as u64,
                state.stderr_truncated,
            );
        }
        ExecAttachSnapshot {
            stdout: Bytes::copy_from_slice(&state.stdout_buf[stdout_offset as usize..]),
            stderr: Bytes::copy_from_slice(&state.stderr_buf[stderr_offset as usize..]),
            stdout_cursor: state.stdout_buf.len() as u64,
            stderr_cursor: state.stderr_buf.len() as u64,
            stdout_len: state.stdout_len,
            stderr_len: state.stderr_len,
            stdout_truncated: state.stdout_truncated,
            stderr_truncated: state.stderr_truncated,
            exit: state.exit,
        }
    };

    let (tx, rx) = mpsc::channel::<ExecEvent>(32);
    let stream_exec_id = exec_id.clone();
    tokio::spawn(async move {
        if !snapshot.stdout.is_empty()
            && tx
                .send(ExecEvent::Stdout(snapshot.stdout.clone()))
                .await
                .is_err()
        {
            return;
        }
        if !snapshot.stderr.is_empty()
            && tx
                .send(ExecEvent::Stderr(snapshot.stderr.clone()))
                .await
                .is_err()
        {
            return;
        }

        let mut stdout_cursor = snapshot.stdout_cursor;
        let mut stderr_cursor = snapshot.stderr_cursor;
        if snapshot.exit.is_none()
            && (snapshot.stdout_len > stdout_cursor || snapshot.stderr_len > stderr_cursor)
        {
            let message = format!(
                "durable exec reattach failed for exec_id {stream_exec_id}: retained \
                 stdout={stdout_cursor} of {} bytes (truncated={}), stderr={stderr_cursor} of {} \
                 bytes (truncated={}); refusing a gapped live replay\n",
                snapshot.stdout_len,
                snapshot.stdout_truncated,
                snapshot.stderr_len,
                snapshot.stderr_truncated,
            );
            send_record_refusal(&tx, message).await;
            return;
        }
        if let Some(exit) = snapshot.exit {
            let _ = tx.send(ExecEvent::Exit(exit)).await;
            return;
        }

        loop {
            match progress.recv().await {
                Ok(ExecRecordEvent::Stdout { offset, bytes }) => {
                    if forward_recorded_output(
                        &tx,
                        &stream_exec_id,
                        "stdout",
                        offset,
                        bytes,
                        &mut stdout_cursor,
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }
                Ok(ExecRecordEvent::Stderr { offset, bytes }) => {
                    if forward_recorded_output(
                        &tx,
                        &stream_exec_id,
                        "stderr",
                        offset,
                        bytes,
                        &mut stderr_cursor,
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                }
                Ok(ExecRecordEvent::Exit(exit)) => {
                    let gap = {
                        let state = record.state.lock();
                        (stdout_cursor < state.stdout_len)
                            .then_some(("stdout", stdout_cursor, state.stdout_len))
                            .or_else(|| {
                                (stderr_cursor < state.stderr_len).then_some((
                                    "stderr",
                                    stderr_cursor,
                                    state.stderr_len,
                                ))
                            })
                    };
                    if let Some((name, cursor, produced)) = gap {
                        send_record_refusal(
                            &tx,
                            format!(
                                "durable exec reattach failed for exec_id {stream_exec_id}: {name} \
                                 replay ended at offset {cursor}, but the command produced \
                                 {produced} bytes; refusing a gapped replay\n"
                            ),
                        )
                        .await;
                    } else {
                        let _ = tx.send(ExecEvent::Exit(exit)).await;
                    }
                    return;
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    match recover_retained_output(
                        &tx,
                        &stream_exec_id,
                        &record,
                        &mut stdout_cursor,
                        &mut stderr_cursor,
                    )
                    .await
                    {
                        Ok(Some(exit)) => {
                            let _ = tx.send(ExecEvent::Exit(exit)).await;
                            return;
                        }
                        Ok(None) => {}
                        Err(()) => return,
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });

    ExecStream {
        sandbox_id,
        exec_id,
        events: Box::pin(ReceiverStream::new(rx)),
    }
}

#[derive(Debug)]
struct ExecAttachSnapshot {
    stdout: Bytes,
    stderr: Bytes,
    stdout_cursor: u64,
    stderr_cursor: u64,
    stdout_len: u64,
    stderr_len: u64,
    stdout_truncated: bool,
    stderr_truncated: bool,
    exit: Option<Option<i32>>,
}

async fn forward_recorded_output(
    tx: &mpsc::Sender<ExecEvent>,
    exec_id: &str,
    name: &str,
    offset: u64,
    bytes: Bytes,
    cursor: &mut u64,
) -> Result<(), ()> {
    let end = offset.saturating_add(bytes.len() as u64);
    if end <= *cursor {
        return Ok(());
    }
    if offset > *cursor {
        send_record_refusal(
            tx,
            format!(
                "durable exec reattach failed for exec_id {exec_id}: {name} replay gap at offset \
                 {}; next available byte is {offset}; refusing a gapped replay\n",
                *cursor
            ),
        )
        .await;
        return Err(());
    }
    let skip = (*cursor - offset) as usize;
    if tx
        .send(match name {
            "stdout" => ExecEvent::Stdout(bytes.slice(skip..)),
            _ => ExecEvent::Stderr(bytes.slice(skip..)),
        })
        .await
        .is_err()
    {
        return Err(());
    }
    *cursor = end;
    Ok(())
}

async fn recover_retained_output(
    tx: &mpsc::Sender<ExecEvent>,
    exec_id: &str,
    record: &ExecRecord,
    stdout_cursor: &mut u64,
    stderr_cursor: &mut u64,
) -> Result<Option<Option<i32>>, ()> {
    let (stdout, stderr, stdout_len, stderr_len, exit) = {
        let state = record.state.lock();
        let stdout = if *stdout_cursor <= state.stdout_buf.len() as u64 {
            Bytes::copy_from_slice(&state.stdout_buf[*stdout_cursor as usize..])
        } else {
            Bytes::new()
        };
        let stderr = if *stderr_cursor <= state.stderr_buf.len() as u64 {
            Bytes::copy_from_slice(&state.stderr_buf[*stderr_cursor as usize..])
        } else {
            Bytes::new()
        };
        (
            stdout,
            stderr,
            state.stdout_len,
            state.stderr_len,
            state.exit,
        )
    };
    if !stdout.is_empty() {
        *stdout_cursor = stdout_cursor.saturating_add(stdout.len() as u64);
        if tx.send(ExecEvent::Stdout(stdout)).await.is_err() {
            return Err(());
        }
    }
    if !stderr.is_empty() {
        *stderr_cursor = stderr_cursor.saturating_add(stderr.len() as u64);
        if tx.send(ExecEvent::Stderr(stderr)).await.is_err() {
            return Err(());
        }
    }
    if stdout_len > *stdout_cursor || stderr_len > *stderr_cursor {
        send_record_refusal(
            tx,
            format!(
                "durable exec reattach failed for exec_id {exec_id}: live consumer lag crossed \
                 the retained output boundary (stdout {} of {stdout_len}, stderr {} of \
                 {stderr_len}); refusing a gapped replay\n",
                *stdout_cursor, *stderr_cursor
            ),
        )
        .await;
        return Err(());
    }
    Ok(exit)
}

async fn send_record_refusal(tx: &mpsc::Sender<ExecEvent>, message: String) {
    let _ = tx.send(ExecEvent::Refused(message)).await;
}

fn refused_offset_stream(
    sandbox_id: SandboxId,
    exec_id: String,
    name: &str,
    requested: u64,
    retained: u64,
    truncated: bool,
) -> ExecStream {
    refused_exec_stream(
        sandbox_id,
        exec_id.clone(),
        format!(
            "durable exec reattach failed for exec_id {exec_id}: requested {name} offset \
             {requested} exceeds retained length {retained} (truncated={truncated}); refusing a \
             gapped replay\n"
        ),
    )
}

fn refused_exec_stream(sandbox_id: SandboxId, exec_id: String, message: String) -> ExecStream {
    ExecStream {
        sandbox_id,
        exec_id,
        events: Box::pin(tokio_stream::iter([ExecEvent::Refused(message)])),
    }
}

fn kill_exec_process_group(pgid: Option<u32>, exec_id: &str) -> Result<(), SandboxError> {
    #[cfg(unix)]
    {
        let pgid = pgid.ok_or_else(|| {
            SandboxError::Vm(format!("process group absent for exec_id {exec_id}").into())
        })?;
        let raw = i32::try_from(pgid).map_err(|error| {
            SandboxError::Vm(format!("invalid process group for {exec_id}: {error}").into())
        })?;
        match nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(raw),
            nix::sys::signal::Signal::SIGKILL,
        ) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(error) => Err(SandboxError::Vm(
                format!("cancel {exec_id}: {error}").into(),
            )),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        Err(SandboxError::Unsupported(
            "cancel_exec requires process groups".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::types::egress::SessionEgressPolicy;
    use engram_core::types::image::SecretMode;
    use engram_core::SessionId;
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
            image_uri: None,
            rootfs_manifest: None,
            cpu: engram_core::types::sandbox::CpuLimit { vcpus: 1 },
            memory: engram_core::types::sandbox::MemoryLimit { max_mib: 256 },
            disk: engram_core::types::sandbox::DiskLimit { max_gib: 1 },
            ttl: None,
            env: HashMap::new(),
            workdir: None,
            network: Default::default(),
            aux_ro_drives: Vec::new(),
        }
    }

    fn exec(argv: &[&str]) -> ExecRequest {
        ExecRequest {
            command: argv.iter().map(|s| s.to_string()).collect(),
            stdin: None,
            env: HashMap::new(),
            workdir: None,
            timeout: None,
            exec_id: None,
            stdout_offset: None,
            stderr_offset: None,
            wake: None,
        }
    }

    #[tokio::test]
    async fn rejects_metadata_delivery_without_interception() {
        use engram_core::types::integration::MetadataFlavor;
        let (backend, _dir) = backend();
        let error = backend
            .notify_session_policy(SessionEgressPolicy {
                session_id: SessionId::new(),
                sandbox_id: SandboxId::new(),
                guest_ip: std::net::Ipv4Addr::LOCALHOST,
                network_allow_hosts: Vec::new(),
                network_allow_host_patterns: Vec::new(),
                allow_all: false,
                secrets: Vec::new(),
                injects: Vec::new(),
                observes: Vec::new(),
                metadata_flavor: Some(MetadataFlavor::Gce),
                secret_mode: SecretMode::Broker,
            })
            .await
            .expect_err("Process must reject a metadata flavor it cannot intercept");

        // The refusal names the flavor, so a second cloud's failure is not
        // reported as Google's.
        let message = error.to_string();
        assert!(message.contains("Gce"), "{message}");
        assert!(
            message.contains("requires host egress interception"),
            "{message}"
        );
    }

    fn durable_exec(exec_id: &str, argv: &[&str]) -> ExecRequest {
        let mut req = exec(argv);
        req.exec_id = Some(exec_id.into());
        req.timeout = Some(Duration::from_secs(3));
        req
    }

    async fn collect_exec_stream(
        mut stream: ExecStream,
    ) -> (Vec<u8>, Vec<u8>, Option<Option<i32>>) {
        use futures::StreamExt;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit = None;
        while let Some(event) = stream.events.next().await {
            match event {
                ExecEvent::Stdout(bytes) => stdout.extend_from_slice(&bytes),
                ExecEvent::Stderr(bytes) => stderr.extend_from_slice(&bytes),
                ExecEvent::Exit(code) => {
                    exit = Some(code);
                    break;
                }
                ExecEvent::Refused(reason) => panic!("exec refused: {reason}"),
            }
        }
        (stdout, stderr, exit)
    }

    async fn collect_refusal(mut stream: ExecStream) -> String {
        use futures::StreamExt;

        let reason = match stream.events.next().await {
            Some(ExecEvent::Refused(reason)) => reason,
            other => panic!("expected Refused terminal, got {other:?}"),
        };
        assert!(
            stream.events.next().await.is_none(),
            "Refused must be the last event"
        );
        reason
    }

    #[tokio::test]
    async fn durable_exec_mid_run_reattach_spawns_once_and_resumes_offsets() {
        use futures::StreamExt;

        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        let req = durable_exec(
            "mid-run-ticket",
            &[
                "sh",
                "-c",
                "printf 'spawn\\n' >> marker; printf first-out; printf first-err >&2; \
                 sleep 0.3; printf second-out; printf second-err >&2; exit 7",
            ],
        );
        let mut first = b.exec_stream(id, req.clone()).await.unwrap();
        let mut prefix_stdout = Vec::new();
        let mut prefix_stderr = Vec::new();
        while prefix_stdout != b"first-out" || prefix_stderr != b"first-err" {
            let event = tokio::time::timeout(Duration::from_secs(2), first.events.next())
                .await
                .expect("first output should arrive before the command exits")
                .expect("first stream should remain open");
            match event {
                ExecEvent::Stdout(bytes) => prefix_stdout.extend_from_slice(&bytes),
                ExecEvent::Stderr(bytes) => prefix_stderr.extend_from_slice(&bytes),
                ExecEvent::Exit(exit) => panic!("command exited before reattach: {exit:?}"),
                ExecEvent::Refused(reason) => panic!("command refused before reattach: {reason}"),
            }
        }

        let mut attach = req;
        attach.stdout_offset = Some(prefix_stdout.len() as u64);
        attach.stderr_offset = Some(prefix_stderr.len() as u64);
        let second = b.exec_stream(id, attach).await.unwrap();
        drop(first);
        let (suffix_stdout, suffix_stderr, exit) = collect_exec_stream(second).await;

        prefix_stdout.extend_from_slice(&suffix_stdout);
        prefix_stderr.extend_from_slice(&suffix_stderr);
        assert_eq!(prefix_stdout, b"first-outsecond-out");
        assert_eq!(prefix_stderr, b"first-errsecond-err");
        assert_eq!(exit, Some(Some(7)));
        let marker = fs::read_to_string(b.cwd_for(id).join("marker")).unwrap();
        assert_eq!(marker.lines().count(), 1, "ticket must spawn exactly once");
    }

    #[tokio::test]
    async fn durable_exec_attach_after_exit_replays_output_and_exact_exit() {
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        let req = durable_exec(
            "completed-ticket",
            &[
                "sh",
                "-c",
                "printf complete-out; printf complete-err >&2; exit 19",
            ],
        );
        let first = collect_exec_stream(b.exec_stream(id, req.clone()).await.unwrap()).await;
        let replay = collect_exec_stream(b.exec_stream(id, req).await.unwrap()).await;

        assert_eq!(first, replay);
        assert_eq!(replay.0, b"complete-out");
        assert_eq!(replay.1, b"complete-err");
        assert_eq!(replay.2, Some(Some(19)));
        b.cancel_exec(id, "completed-ticket".into())
            .await
            .expect("cancelling an exited record is a no-op");
    }

    /// PR #874 review class ("retention without a bound"): completed records
    /// beyond the retention cap are evicted oldest-first, so a long-lived
    /// sandbox cannot accumulate output buffers indefinitely. Running
    /// records are untouched, and an evicted ticket behaves like a TTL'd
    /// guest journal: nonzero-offset attaches refuse loudly.
    #[tokio::test]
    async fn completed_records_beyond_retention_evict_oldest_first() {
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        for i in 0..(EXEC_COMPLETED_RETENTION + 2) {
            let req = durable_exec(&format!("retained-{i}"), &["sh", "-c", "printf done"]);
            let (out, _, exit) = collect_exec_stream(b.exec_stream(id, req).await.unwrap()).await;
            assert_eq!((out.as_slice(), exit), (b"done".as_slice(), Some(Some(0))));
        }

        // The two oldest completed tickets were evicted: a nonzero-offset
        // attach is the missing-record refusal, never a respawn.
        let mut evicted = durable_exec("retained-0", &["sh", "-c", "printf done"]);
        evicted.stdout_offset = Some(4);
        let reason = collect_refusal(b.exec_stream(id, evicted).await.unwrap()).await;
        assert!(
            reason.contains("refusing to spawn a second command"),
            "evicted ticket must refuse, got: {reason}"
        );

        // The newest ticket is still fully replayable.
        let newest = durable_exec(
            &format!("retained-{}", EXEC_COMPLETED_RETENTION + 1),
            &["sh", "-c", "printf done"],
        );
        let (out, _, exit) = collect_exec_stream(b.exec_stream(id, newest).await.unwrap()).await;
        assert_eq!((out.as_slice(), exit), (b"done".as_slice(), Some(Some(0))));
    }

    #[tokio::test]
    async fn attaching_oldest_completed_record_exempts_it_from_pruning() {
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        let oldest_command = "printf 'spawn\\n' >> marker; printf oldest";

        for i in 0..=EXEC_COMPLETED_RETENTION {
            let command = if i == 0 {
                oldest_command.to_string()
            } else {
                format!("printf 'spawn\\n' >> marker; printf record-{i}")
            };
            let req = durable_exec(&format!("retained-attach-{i}"), &["sh", "-c", &command]);
            let (_, _, exit) = collect_exec_stream(b.exec_stream(id, req).await.unwrap()).await;
            assert_eq!(exit, Some(Some(0)));
        }
        let spawn_count_before = fs::read_to_string(b.cwd_for(id).join("marker"))
            .unwrap()
            .lines()
            .count();
        assert_eq!(spawn_count_before, EXEC_COMPLETED_RETENTION + 1);

        let replay = durable_exec("retained-attach-0", &["sh", "-c", oldest_command]);
        let (stdout, stderr, exit) =
            collect_exec_stream(b.exec_stream(id, replay).await.unwrap()).await;

        assert_eq!(stdout, b"oldest");
        assert!(stderr.is_empty());
        assert_eq!(exit, Some(Some(0)));
        let spawn_count_after = fs::read_to_string(b.cwd_for(id).join("marker"))
            .unwrap()
            .lines()
            .count();
        assert_eq!(
            spawn_count_after, spawn_count_before,
            "attaching a still-present completed record must replay instead of pruning and respawning it"
        );
    }

    #[tokio::test]
    async fn durable_exec_command_mismatch_refuses_without_second_spawn() {
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        let first = durable_exec(
            "mismatch-ticket",
            &["sh", "-c", "printf 'first\\n' >> marker"],
        );
        let (_, _, exit) = collect_exec_stream(b.exec_stream(id, first).await.unwrap()).await;
        assert_eq!(exit, Some(Some(0)));

        let mismatch = durable_exec(
            "mismatch-ticket",
            &["sh", "-c", "printf 'second\\n' >> marker"],
        );
        let reason = collect_refusal(b.exec_stream(id, mismatch).await.unwrap()).await;
        assert!(reason.contains("first writer wins"), "{reason}");
        assert!(reason.contains("refusing different command"), "{reason}");
        assert_eq!(
            fs::read_to_string(b.cwd_for(id).join("marker")).unwrap(),
            "first\n"
        );
    }

    #[tokio::test]
    async fn durable_exec_unknown_ticket_with_offsets_refuses_without_spawn() {
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        let mut req = durable_exec(
            "missing-ticket",
            &["sh", "-c", "printf spawned > must-not-exist"],
        );
        req.stdout_offset = Some(1);
        let reason = collect_refusal(b.exec_stream(id, req).await.unwrap()).await;
        assert!(reason.contains("record for exec_id missing-ticket is missing"));
        assert!(reason.contains("refusing to spawn a second command"));
        assert!(!b.cwd_for(id).join("must-not-exist").exists());
    }

    #[tokio::test]
    async fn durable_exec_cancel_kills_the_process_group() {
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        let stream = b
            .exec_stream(
                id,
                durable_exec(
                    "cancel-ticket",
                    &[
                        "sh",
                        "-c",
                        "sleep 30 & child=$!; printf %s \"$child\" > child.pid; wait \"$child\"",
                    ],
                ),
            )
            .await
            .unwrap();
        // Wait for a PARSEABLE pid, not merely for the file to exist: `>`
        // creates child.pid before `printf` writes to it, so an existence-only
        // poll reads an empty file and panics with ParseIntError::Empty.
        let child_pid_path = b.cwd_for(id).join("child.pid");
        let mut child_pid = None;
        for _ in 0..100 {
            child_pid = fs::read_to_string(&child_pid_path)
                .ok()
                .and_then(|raw| raw.trim().parse::<i32>().ok());
            if child_pid.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let child_pid = child_pid.expect("shell should publish its child pid");

        b.cancel_exec(id, "cancel-ticket".into()).await.unwrap();
        let (_, _, exit) =
            tokio::time::timeout(Duration::from_secs(2), collect_exec_stream(stream))
                .await
                .expect("cancelled process group should close both pipes promptly");
        assert_eq!(exit, Some(None));

        let mut child_alive = true;
        for _ in 0..100 {
            child_alive = std::process::Command::new("kill")
                .args(["-0", &child_pid.to_string()])
                .status()
                .is_ok_and(|status| status.success());
            if !child_alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!child_alive, "child process {child_pid} survived cancel");
    }

    #[tokio::test]
    async fn durable_exec_dropped_consumer_does_not_kill_command() {
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        let req = durable_exec(
            "drop-ticket",
            &[
                "sh",
                "-c",
                "printf 'spawn\\n' >> drop-marker; sleep 0.2; printf survived",
            ],
        );
        drop(b.exec_stream(id, req.clone()).await.unwrap());
        tokio::time::sleep(Duration::from_millis(350)).await;

        let (stdout, stderr, exit) =
            collect_exec_stream(b.exec_stream(id, req).await.unwrap()).await;
        assert_eq!(stdout, b"survived");
        assert!(stderr.is_empty());
        assert_eq!(exit, Some(Some(0)));
        assert_eq!(
            fs::read_to_string(b.cwd_for(id).join("drop-marker"))
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn durable_exec_offset_beyond_retained_output_refuses_gap() {
        let (b, _d) = backend();
        let id = b.create(spec()).await.unwrap();
        let req = durable_exec("gap-ticket", &["sh", "-c", "printf abc"]);
        let _ = collect_exec_stream(b.exec_stream(id, req.clone()).await.unwrap()).await;

        let mut attach = req;
        attach.stdout_offset = Some(4);
        let reason = collect_refusal(b.exec_stream(id, attach).await.unwrap()).await;
        assert!(reason.contains("requested stdout offset 4"));
        assert!(reason.contains("retained length 3"));
        assert!(reason.contains("gapped replay"));
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
            exec_id: None,
            stdout_offset: None,
            stderr_offset: None,
            wake: None,
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
            exec_id: None,
            stdout_offset: None,
            stderr_offset: None,
            wake: None,
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
            exec_id: None,
            stdout_offset: None,
            stderr_offset: None,
            wake: None,
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
            exec_id: None,
            stdout_offset: None,
            stderr_offset: None,
            wake: None,
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
            binding_epoch: 1,
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!("echo $$ > {pid_file}; exec sleep 60"),
            ],
            env: HashMap::new(),
            session_env: HashMap::new(),
            host_ca_pem: None,
        };
        b.start_agent(id, agent).await.unwrap();

        // Wait for the agent to write its pid file. Poll for CONTENT, not
        // existence: `echo $$ > file` opens (creates) the file before the
        // write lands, so an existence check can observe an empty file and
        // the parse below dies with `ParseIntError { kind: Empty }` (flaked
        // in CI 2026-07-31). Real-world race budgets are tiny here; a few
        // hundred ms is plenty.
        let pid_path = b.cwd_for(id).join(pid_file);
        let mut pid_contents = String::new();
        for _ in 0..50 {
            pid_contents = fs::read_to_string(&pid_path).unwrap_or_default();
            if !pid_contents.trim().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let pid: i32 = pid_contents
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

        // ADR 0007 Phase 6: backend owns staging — we no longer pass
        // a snap_dir; look it up post-snapshot via snapshot_path_for
        // to assert on the files it wrote.
        let meta = b.snapshot(id).await.unwrap();
        let snap_dir = b.snapshot_path_for(meta.id);
        assert!(meta.size_bytes > 0);
        assert_eq!(meta.image_version, "warm-test");
        assert!(snap_dir.join("manifest.json").is_file());
        assert!(snap_dir.join("fs.tar.gz").is_file());

        // Destroy the original so any restore reads from the snapshot,
        // not from leftover state.
        b.destroy(id).await.unwrap();

        let restored = b.restore(meta).await.unwrap();
        let h_top = b.exec(restored, exec(&["cat", "greeting"])).await.unwrap();
        assert_eq!(String::from_utf8(h_top.stdout).unwrap(), "hello\n");
        let h_nested = b.exec(restored, exec(&["cat", "sub/file"])).await.unwrap();
        assert_eq!(String::from_utf8(h_nested.stdout).unwrap(), "nested\n");
    }

    #[tokio::test]
    async fn snapshot_unknown_id_is_not_found() {
        let (b, _d) = backend();
        let res = b.snapshot(SandboxId::new()).await;
        assert!(matches!(res, Err(SandboxError::NotFound)));
    }

    #[tokio::test]
    async fn restore_with_corrupted_manifest_errors_cleanly() {
        let (b, _d) = backend();
        // Plant a corrupted manifest in the path the backend would
        // look up for this snapshot_id, then call restore — the
        // backend resolves the dir internally now, so we just need
        // metadata with the right id.
        let snap_id = SnapshotId::new();
        let snap_dir = b.snapshot_path_for(snap_id);
        std::fs::create_dir_all(&snap_dir).unwrap();
        fs::write(snap_dir.join("manifest.json"), b"not json").unwrap();
        fs::write(snap_dir.join("fs.tar.gz"), b"").unwrap();
        let meta = SnapshotMetadata {
            migration_source: None,
            id: snap_id,
            size_bytes: 0,
            created_at: chrono::Utc::now(),
            image_version: "test".into(),
            disk_manifest: None,
            memory_manifest: None,
            base_memory_manifest: None,
            source_sandbox_id: None,
            state_blob_key: None,
            sidecar_blob_key: None,
            rootfs_blob_key: None,
            working_set_blob_key: None,
            aux_bundles: vec![],
            paused_at: None,
            peer_hints: Vec::new(),
        };
        let res = b.restore(meta).await;
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
                ExecEvent::Refused(reason) => panic!("streaming command refused: {reason}"),
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
            exec_id: None,
            stdout_offset: None,
            stderr_offset: None,
            wake: None,
        };
        let h = b.exec(id, req).await.unwrap();
        let pwd = String::from_utf8(h.stdout).unwrap();
        assert!(
            pwd.trim().ends_with("deep/nested"),
            "pwd = {pwd:?} should end at the requested workdir",
        );
    }
}
