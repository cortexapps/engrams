//! `checkpoint_session` — durable backstop for git-backed sessions.
//!
//! Two atomic-ish writes per checkpoint:
//!
//!   1. Send [`HarnessCommand::Checkpoint`] over the harness channel
//!      so the in-guest agent gets a chance to flush its transcript
//!      to disk before we capture the workspace.
//!   2. Run `git add -A && git commit && git push origin <branch>`
//!      via the [`SandboxBackend`]'s exec channel, capturing the
//!      resulting commit SHA.
//!
//! Step 1 is best-effort — `engram-harness-noop` and most production
//! adapters (Claude Code) flush their transcript synchronously per
//! tool call, so the checkpoint command is largely advisory. Step 2
//! is the durable bit: once `git push` returns 0, the workspace
//! state is on the remote and a fresh sandbox can clone the
//! checkpoint branch to resume.
//!
//! No-op for sessions whose `SessionKind` isn't `Git` (Local /
//! Readonly sessions have no checkpoint branch). Callers that don't
//! know the kind should consult their session row first; this
//! primitive accepts only the params it needs.

use std::sync::Arc;
use std::time::Duration;

use engram_core::traits::SandboxBackend;
use engram_core::types::sandbox::ExecRequest;
use engram_core::{SandboxError, SandboxId};
use engram_harness_proto::CheckpointReason;

use crate::harness::{HarnessError, HarnessHub};

/// Workspace-pending-changes hard cap. Exceed and the checkpoint
/// refuses with [`CheckpointError::WorkspaceTooLarge`] before pushing
/// — keeps a single accidental `tar -xf big-thing.tar.gz` in the
/// workspace from blowing past a 30s preemption budget.
///
/// Checked by parsing `git status --porcelain` byte counts and
/// summing per-file `git ls-files --others --exclude-standard -z`
/// sizes. Operators can override with `ENGRAM_MAX_CHECKPOINT_BYTES`
/// (Track B/C/E wires the env-var read).
pub const DEFAULT_MAX_CHECKPOINT_BYTES: u64 = 100 * 1024 * 1024;

/// Knobs for one checkpoint call. Callers normally use defaults.
#[derive(Clone, Debug)]
pub struct CheckpointConfig {
    /// Wall-clock budget for the harness `Checkpoint` ack. Past this
    /// we proceed with the git push anyway — the harness is
    /// presumably stuck mid-tool-call, but the workspace state is
    /// still durable.
    pub harness_ack_timeout: Duration,
    /// Pending-changes byte cap; checkpoints exceeding this fail
    /// with `WorkspaceTooLarge`.
    pub max_checkpoint_bytes: u64,
    /// `user.email` to use on the synthetic `git commit`. Stored on
    /// the commit; doesn't have to resolve to anything real.
    pub committer_email: String,
    /// `user.name` for the synthetic commit.
    pub committer_name: String,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            harness_ack_timeout: Duration::from_secs(5),
            max_checkpoint_bytes: DEFAULT_MAX_CHECKPOINT_BYTES,
            committer_email: "engram-checkpointer@engram.local".into(),
            committer_name: "Engram Checkpointer".into(),
        }
    }
}

/// Outcome of a successful checkpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointOutcome {
    /// SHA of the new commit on `branch`. `None` only when the
    /// pre-existing branch HEAD already covered the workspace state
    /// (no `git diff` between staged and HEAD) — the checkpoint
    /// emits an empty `--allow-empty` commit anyway, so today this
    /// is always Some(_). Reserved for a future "skip-empty" config.
    pub commit_sha: Option<String>,
    /// True when the harness ack'd the checkpoint command before the
    /// git push ran. False when the timeout elapsed or the harness
    /// wasn't attached. Surfaced to operators in the
    /// `SessionEvent::CheckpointPushed` event so they can correlate
    /// "transcript on the remote may be one tool call behind".
    pub harness_acked: bool,
}

/// Run a checkpoint against `sandbox_id`'s workspace.
///
/// `workspace` is the path inside the sandbox where the working
/// tree lives (`/workspace` in production; tempdirs in tests).
/// `branch` is the destination branch on `origin` — typically
/// `engram/sessions/<id>` for the writable repo session.
/// `reason` is informational; surfaces in the synthetic commit
/// message for operator forensics.
pub async fn checkpoint_session(
    hub: &HarnessHub,
    backend: &Arc<dyn SandboxBackend>,
    sandbox_id: SandboxId,
    workspace: &str,
    branch: &str,
    reason: CheckpointReason,
    cfg: &CheckpointConfig,
) -> Result<CheckpointOutcome, CheckpointError> {
    // Ask the harness to flush its transcript. Don't block the
    // push if it doesn't ack — workspace durability is the
    // contract; transcript freshness is an opportunistic bonus.
    let harness_acked =
        match tokio::time::timeout(cfg.harness_ack_timeout, hub.checkpoint(sandbox_id, reason))
            .await
        {
            Ok(Ok(())) => true,
            Ok(Err(HarnessError::NotAttached)) => false,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "harness checkpoint failed; proceeding with git push");
                false
            }
            Err(_) => {
                tracing::warn!("harness checkpoint timed out; proceeding with git push");
                false
            }
        };

    let outcome = checkpoint_workspace_only(backend, sandbox_id, workspace, branch, reason, cfg)
        .await?
        .with_harness_acked(harness_acked);
    Ok(outcome)
}

/// Run only the git-push portion of a checkpoint, skipping the
/// harness `Checkpoint` round-trip.
///
/// Used by Track C.9's auto-checkpoint path: the harness has just
/// emitted `HarnessEvent::Idle` / `RunCompleted`, so we already
/// know it's at a safe boundary — no need to re-flush.
/// `harness_acked` on the returned [`CheckpointOutcome`] is `true`
/// since the harness implicitly acked by going idle. Manual /
/// preemption checkpoints continue to use [`checkpoint_session`]
/// which adds the explicit harness round-trip.
pub async fn checkpoint_workspace_only(
    backend: &Arc<dyn SandboxBackend>,
    sandbox_id: SandboxId,
    workspace: &str,
    branch: &str,
    reason: CheckpointReason,
    cfg: &CheckpointConfig,
) -> Result<CheckpointOutcome, CheckpointError> {
    // Workspace size guard. `git status --porcelain` gives us a
    // manageable byte count of the diff plus untracked files; we
    // read it and gate on its size. Doesn't catch a 500MB file
    // tucked into a tracked directory, but for the common runaway-
    // log pattern (a `.log` the agent writes) it surfaces the
    // problem before the push.
    let status = run_git(
        backend,
        sandbox_id,
        workspace,
        &["git", "status", "--porcelain", "-z"],
    )
    .await?;
    if status.stdout.len() as u64 > cfg.max_checkpoint_bytes {
        return Err(CheckpointError::WorkspaceTooLarge {
            pending_bytes: status.stdout.len() as u64,
            limit: cfg.max_checkpoint_bytes,
        });
    }

    // Skip empty checkpoints: if the agent's "completed run" produced
    // zero workspace changes (read-only run, or every change reverted
    // before the agent went idle), don't bother committing or pushing.
    // Returns Ok with commit_sha = None so callers can distinguish
    // "checkpointed nothing" from "pushed a new commit". Saves a
    // round-trip per idle event for read-only agents and keeps the
    // checkpoint branch's `git log` aligned with actual progress.
    if status.stdout.is_empty() {
        return Ok(CheckpointOutcome {
            commit_sha: None,
            harness_acked: true,
        });
    }

    // Stage every change, commit, push to the session branch.
    run_git(backend, sandbox_id, workspace, &["git", "add", "-A"]).await?;

    let message = format!("engram-checkpoint {}", checkpoint_reason_str(reason));
    let commit_args = [
        "git",
        "-c",
        &format!("user.email={}", cfg.committer_email),
        "-c",
        &format!("user.name={}", cfg.committer_name),
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-m",
        &message,
    ];
    let _ = run_git(backend, sandbox_id, workspace, &commit_args).await?;

    // Capture the commit SHA so the caller can emit it as part of
    // the SessionEvent::CheckpointPushed broadcast.
    let rev = run_git(
        backend,
        sandbox_id,
        workspace,
        &["git", "rev-parse", "HEAD"],
    )
    .await?;
    let commit_sha = String::from_utf8_lossy(&rev.stdout).trim().to_string();

    // Push to the session branch. `HEAD:<branch>` lets us push
    // without ever creating a local tracking branch — the workspace
    // history stays linear on whatever the agent was working on,
    // and we only publish the snapshot to the engram-controlled
    // namespace.
    let push_args = ["git", "push", "origin", &format!("HEAD:{branch}")];
    run_git(backend, sandbox_id, workspace, &push_args).await?;

    Ok(CheckpointOutcome {
        commit_sha: Some(commit_sha),
        harness_acked: true,
    })
}

impl CheckpointOutcome {
    fn with_harness_acked(mut self, acked: bool) -> Self {
        self.harness_acked = acked;
        self
    }
}

#[derive(Debug)]
pub enum CheckpointError {
    /// One of the git commands exited non-zero. `stage` is which
    /// command failed (e.g. "git push origin ..."), `code` is its
    /// exit status, and `stderr` is the captured stderr (UTF-8
    /// lossy; binary noise is replaced).
    GitFailed {
        stage: String,
        code: Option<i32>,
        stderr: String,
    },
    /// Pending changes exceed the configured cap. The agent
    /// probably wrote a runaway file (build artifact, log) into the
    /// workspace; operator should fix `.gitignore` and retry.
    WorkspaceTooLarge {
        pending_bytes: u64,
        limit: u64,
    },
    Sandbox(SandboxError),
}

impl std::fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GitFailed {
                stage,
                code,
                stderr,
            } => write!(
                f,
                "checkpoint {stage} failed (exit {code:?}): {}",
                stderr.trim()
            ),
            Self::WorkspaceTooLarge {
                pending_bytes,
                limit,
            } => write!(
                f,
                "workspace has {pending_bytes} bytes of pending changes; \
                 over the {limit}-byte checkpoint cap"
            ),
            Self::Sandbox(e) => write!(f, "checkpoint sandbox error: {e}"),
        }
    }
}

impl std::error::Error for CheckpointError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sandbox(e) => Some(e),
            _ => None,
        }
    }
}

impl From<SandboxError> for CheckpointError {
    fn from(e: SandboxError) -> Self {
        Self::Sandbox(e)
    }
}

/// One git invocation through the SandboxBackend's buffered exec.
///
/// Passes `-c core.fsmonitor=false -c gc.auto=0` and an isolated env
/// (`GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`,
/// `GIT_OPTIONAL_LOCKS=0`, `GIT_TERMINAL_PROMPT=0`) to every
/// invocation:
///
/// - **fsmonitor** spawns a long-lived helper daemon that watches the
///   working tree via fsevents/inotify. We don't want that in a
///   short-lived sandbox; if `materialize_rootfs` copied a stale
///   fsmonitor IPC socket from the host's git, the sandbox's git
///   will try to talk to a daemon that doesn't exist (or worse, one
///   shared with other parallel tests) and hang.
/// - **gc.auto=0** suppresses the surprise "Auto packing the
///   repository for optimum performance" runs that can fire on
///   `commit` / `push` and slow down a checkpoint.
/// - **GIT_CONFIG_GLOBAL/NOSYSTEM** hermetically seal off the user's
///   global gitconfig — production sandboxes have no `~/.gitconfig`
///   anyway, but on the dev path (ProcessBackend on macOS) the host
///   user's config bleeds in and can carry hostile fsmonitor / signing
///   defaults.
/// - **GIT_OPTIONAL_LOCKS=0** stops `git status` from holding a write
///   lock on the index when it's read-only; with parallel checkpoints
///   touching the same workspace this avoids spurious "Another git
///   process seems to be running" errors.
/// - **GIT_TERMINAL_PROMPT=0** turns any credential / passphrase
///   prompt into a hard error instead of a hang on `git push`.
async fn run_git(
    backend: &Arc<dyn SandboxBackend>,
    sandbox_id: SandboxId,
    workspace: &str,
    argv: &[&str],
) -> Result<engram_core::types::sandbox::ExecHandle, CheckpointError> {
    let mut command: Vec<String> = Vec::with_capacity(argv.len() + 4);
    command.push(argv[0].to_string()); // "git"
    command.push("-c".into());
    command.push("core.fsmonitor=false".into());
    command.push("-c".into());
    command.push("gc.auto=0".into());
    for a in &argv[1..] {
        command.push(a.to_string());
    }
    let mut env = std::collections::HashMap::new();
    env.insert("GIT_CONFIG_GLOBAL".into(), "/dev/null".into());
    env.insert("GIT_CONFIG_NOSYSTEM".into(), "1".into());
    env.insert("GIT_OPTIONAL_LOCKS".into(), "0".into());
    env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());

    let req = ExecRequest {
        command,
        stdin: None,
        env,
        workdir: Some(workspace.to_string()),
        timeout: Some(Duration::from_secs(60)),
    };
    let handle = backend.exec(sandbox_id, req).await?;
    if handle.exit_status != Some(0) {
        return Err(CheckpointError::GitFailed {
            stage: argv.join(" "),
            code: handle.exit_status,
            stderr: String::from_utf8_lossy(&handle.stderr).into_owned(),
        });
    }
    Ok(handle)
}

fn checkpoint_reason_str(reason: CheckpointReason) -> &'static str {
    match reason {
        CheckpointReason::Idle => "idle",
        CheckpointReason::Preempt => "preempt",
        CheckpointReason::Manual => "manual",
        CheckpointReason::RunCompleted => "run-completed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{EventSink, HarnessHub};
    use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec};
    use engram_harness_proto::HarnessEvent;
    use engram_sandbox_process::ProcessBackend;
    use std::path::Path;
    use std::process::Command as StdCommand;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn null_sink() -> EventSink {
        Arc::new(|_, _, _: HarnessEvent| Box::new(Box::pin(async {})))
    }

    /// Run a host-side command for test setup, with the same hermetic
    /// git environment the production primitive uses. Specifically
    /// `GIT_CONFIG_GLOBAL=/dev/null` to keep the dev user's
    /// `~/.gitconfig` (which on macOS often enables fsmonitor by
    /// default) from leaking into the test repo's `.git/config`.
    /// Without this, `cp -c -R` of `.git/` would copy a stale
    /// `fsmonitor--daemon.ipc` socket and the sandbox's git would
    /// hang trying to talk to it.
    fn run_host(args: &[&str], cwd: &Path) {
        let out = StdCommand::new(args[0])
            .args(&args[1..])
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("spawn host command");
        assert!(
            out.status.success(),
            "host {} failed in {}: {}",
            args.join(" "),
            cwd.display(),
            String::from_utf8_lossy(&out.stderr),
        );
    }

    /// Build a `(remote.git, seed_workspace)` pair: a bare repo (the
    /// "remote") plus a working clone with one initial commit on
    /// `main` already pushed. The checkpoint test starts from
    /// `seed_workspace` as the sandbox's rootfs_source.
    fn seed_remote_and_workspace() -> (TempDir, TempDir) {
        let remote = TempDir::new().expect("remote dir");
        let workspace = TempDir::new().expect("workspace dir");

        run_host(
            &["git", "init", "--bare", "--initial-branch=main"],
            remote.path(),
        );

        run_host(&["git", "init", "--initial-branch=main"], workspace.path());
        run_host(
            &[
                "git",
                "remote",
                "add",
                "origin",
                &format!("{}", remote.path().display()),
            ],
            workspace.path(),
        );
        run_host(
            &[
                "git",
                "-c",
                "user.email=test@engram.local",
                "-c",
                "user.name=test",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
            workspace.path(),
        );
        run_host(&["git", "push", "-u", "origin", "main"], workspace.path());

        (remote, workspace)
    }

    fn process_spec_with_rootfs(rootfs: &Path) -> SandboxSpec {
        SandboxSpec {
            image: "checkpoint-test".into(),
            rootfs_source: Some(rootfs.to_path_buf()),
            cpu: CpuLimit { vcpus: 1 },
            memory: MemoryLimit { max_mib: 256 },
            disk: DiskLimit { max_gib: 1 },
            ttl: None,
            env: Default::default(),
            workdir: None,
            harness_substrate: None,
        }
    }

    /// Verify the bare remote contains the expected branch with at
    /// least one commit beyond the initial.
    fn assert_branch_has_commits(remote: &Path, branch: &str) -> String {
        let out = StdCommand::new("git")
            .args(["-C"])
            .arg(remote)
            .args(["rev-list", "--count", branch])
            .output()
            .expect("rev-list");
        assert!(out.status.success(), "branch {branch} not on remote");
        let count: u32 = String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .expect("rev-list count");
        assert!(count >= 1, "expected >=1 commit on {branch}, got {count}",);
        // Capture the HEAD SHA on that branch so the test can
        // compare to the checkpoint outcome.
        let sha_out = StdCommand::new("git")
            .args(["-C"])
            .arg(remote)
            .args(["rev-parse", branch])
            .output()
            .expect("rev-parse");
        String::from_utf8_lossy(&sha_out.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn checkpoint_pushes_workspace_changes_to_session_branch() {
        let (remote, workspace) = seed_remote_and_workspace();

        let sandbox_root = TempDir::new().expect("sandbox root");
        let backend: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(sandbox_root.path()));

        // Sandbox cwd starts as a copy of `workspace` (which has
        // origin pointing at the bare remote). The checkpoint
        // primitive runs git inside that copy.
        let sandbox_id = backend
            .create(process_spec_with_rootfs(workspace.path()))
            .await
            .expect("create");

        // Make a new file in the sandbox cwd via exec — simulating
        // the agent's tool calls modifying the workspace.
        let req = ExecRequest {
            command: vec!["sh".into(), "-c".into(), "echo hello > new-file.txt".into()],
            stdin: None,
            env: Default::default(),
            workdir: None,
            timeout: Some(Duration::from_secs(5)),
        };
        let h = backend.exec(sandbox_id, req).await.unwrap();
        assert_eq!(h.exit_status, Some(0));

        let hub = HarnessHub::new(null_sink());
        let cfg = CheckpointConfig::default();
        let outcome = checkpoint_session(
            &hub,
            &backend,
            sandbox_id,
            ".",
            "engram/sessions/test-1",
            CheckpointReason::Manual,
            &cfg,
        )
        .await
        .expect("checkpoint");

        assert!(
            outcome.commit_sha.is_some(),
            "successful checkpoint must report the new commit SHA"
        );
        assert!(
            !outcome.harness_acked,
            "no harness attached → not ack'd, but the push still ran"
        );

        let remote_sha = assert_branch_has_commits(remote.path(), "engram/sessions/test-1");
        assert_eq!(
            remote_sha,
            outcome.commit_sha.as_deref().unwrap(),
            "remote branch HEAD must match the SHA the checkpointer reported"
        );
    }

    #[tokio::test]
    async fn checkpoint_refuses_when_workspace_diff_exceeds_cap() {
        let (_remote, workspace) = seed_remote_and_workspace();

        let sandbox_root = TempDir::new().expect("sandbox root");
        let backend: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(sandbox_root.path()));
        let sandbox_id = backend
            .create(process_spec_with_rootfs(workspace.path()))
            .await
            .expect("create");

        // Create enough untracked files that `git status --porcelain
        // -z` output exceeds the (intentionally tight) 100-byte cap.
        // Each line is roughly `?? <path>\0`, so ~10 short-named
        // files puts us comfortably over.
        let req = ExecRequest {
            command: vec![
                "sh".into(),
                "-c".into(),
                "for i in $(seq 1 50); do touch \"runaway-$i.log\"; done".into(),
            ],
            stdin: None,
            env: Default::default(),
            workdir: None,
            timeout: Some(Duration::from_secs(5)),
        };
        let h = backend.exec(sandbox_id, req).await.unwrap();
        assert_eq!(h.exit_status, Some(0));

        let hub = HarnessHub::new(null_sink());
        let cfg = CheckpointConfig {
            max_checkpoint_bytes: 100, // tight cap so a single-file diff trips it
            ..CheckpointConfig::default()
        };

        let err = checkpoint_session(
            &hub,
            &backend,
            sandbox_id,
            ".",
            "engram/sessions/test-cap",
            CheckpointReason::Manual,
            &cfg,
        )
        .await
        .expect_err("expected WorkspaceTooLarge");
        match err {
            CheckpointError::WorkspaceTooLarge {
                pending_bytes,
                limit,
            } => {
                assert!(pending_bytes > limit);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn checkpoint_surfaces_git_push_failure_with_stage_and_stderr() {
        // Wire up a sandbox whose origin points at a *non-existent*
        // bare repo. The push will fail; the error must call out
        // exactly which stage failed.
        let workspace = TempDir::new().expect("workspace");
        run_host(&["git", "init", "--initial-branch=main"], workspace.path());
        run_host(
            &[
                "git",
                "-c",
                "user.email=t@e.local",
                "-c",
                "user.name=t",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
            workspace.path(),
        );
        run_host(
            &["git", "remote", "add", "origin", "/no/such/path/remote.git"],
            workspace.path(),
        );

        let sandbox_root = TempDir::new().expect("sandbox root");
        let backend: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(sandbox_root.path()));
        let sandbox_id = backend
            .create(process_spec_with_rootfs(workspace.path()))
            .await
            .expect("create");

        // Make the workspace dirty so the checkpoint primitive
        // doesn't short-circuit on the empty-diff guard.
        let req = ExecRequest {
            command: vec!["sh".into(), "-c".into(), "echo dirty > note.txt".into()],
            stdin: None,
            env: Default::default(),
            workdir: None,
            timeout: Some(Duration::from_secs(5)),
        };
        backend.exec(sandbox_id, req).await.unwrap();

        let hub = HarnessHub::new(null_sink());
        let err = checkpoint_session(
            &hub,
            &backend,
            sandbox_id,
            ".",
            "engram/sessions/test-broken-remote",
            CheckpointReason::Manual,
            &CheckpointConfig::default(),
        )
        .await
        .expect_err("expected GitFailed");
        match err {
            CheckpointError::GitFailed { stage, .. } => {
                assert!(
                    stage.contains("git push"),
                    "expected the push stage to surface; got: {stage}"
                );
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn checkpoint_workspace_only_skips_when_diff_is_empty() {
        // Idle-triggered auto-checkpoint with no workspace changes
        // should return Ok with commit_sha=None and not push at all.
        let (_remote, workspace) = seed_remote_and_workspace();

        let sandbox_root = TempDir::new().expect("sandbox root");
        let backend: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(sandbox_root.path()));
        let sandbox_id = backend
            .create(process_spec_with_rootfs(workspace.path()))
            .await
            .expect("create");

        // Don't touch the workspace; its diff against the seed clone
        // is empty.
        let outcome = checkpoint_workspace_only(
            &backend,
            sandbox_id,
            ".",
            "engram/sessions/idle-no-change",
            CheckpointReason::RunCompleted,
            &CheckpointConfig::default(),
        )
        .await
        .expect("checkpoint");
        assert!(outcome.commit_sha.is_none(), "no diff → no commit");
        assert!(outcome.harness_acked, "auto path is implicitly ack'd");
    }

    #[tokio::test]
    async fn checkpoint_workspace_only_pushes_when_diff_present() {
        let (remote, workspace) = seed_remote_and_workspace();

        let sandbox_root = TempDir::new().expect("sandbox root");
        let backend: Arc<dyn SandboxBackend> = Arc::new(ProcessBackend::new(sandbox_root.path()));
        let sandbox_id = backend
            .create(process_spec_with_rootfs(workspace.path()))
            .await
            .expect("create");

        let req = ExecRequest {
            command: vec![
                "sh".into(),
                "-c".into(),
                "echo idle-test > completed.txt".into(),
            ],
            stdin: None,
            env: Default::default(),
            workdir: None,
            timeout: Some(Duration::from_secs(5)),
        };
        backend.exec(sandbox_id, req).await.unwrap();

        let outcome = checkpoint_workspace_only(
            &backend,
            sandbox_id,
            ".",
            "engram/sessions/idle-with-change",
            CheckpointReason::RunCompleted,
            &CheckpointConfig::default(),
        )
        .await
        .expect("checkpoint");
        let sha = outcome.commit_sha.expect("non-empty diff → commit");
        let remote_sha =
            assert_branch_has_commits(remote.path(), "engram/sessions/idle-with-change");
        assert_eq!(sha, remote_sha);
    }
}
