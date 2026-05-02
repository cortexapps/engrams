//! Coord-local bare git clones — Track F's foundation.
//!
//! The git-native CLI verbs (`engram session log/diff/fork`) need a
//! place to run `git log`, `git diff`, and `git push <sha>:<branch>`
//! against the writable repo without involving a session sandbox.
//! The simplest model: keep a bare clone of every writable repo
//! Engram has touched, under
//! `<local_path>/git-workdirs/<sha256(url)>/`. Each session-level
//! query lazily fetches into the bare clone, then runs the read.
//!
//! - **Bare**, not a working tree: we never check out files; we just
//!   need ref + commit metadata. `git log/diff` work fine on bare
//!   repos. `git push` to a bare repo works.
//! - **Per-URL key**, not per-session: many sessions share one
//!   writable repo, so the bake's amortised across them.
//! - **Lazy**: an Engram instance that never runs `engram session
//!   log` against repo X never clones it.
//! - **Concurrent-fetch-safe**: a per-URL `tokio::Mutex` serialises
//!   fetches against the same bare clone. Different repos clone in
//!   parallel.
//!
//! Production path: same shape, just the local_path is in a
//! persistent disk so coord restarts don't re-clone everything.
//! Coord redeploys do re-clone (image rebuilds wipe local state) —
//! acceptable since clones are bandwidth-only and amortised across
//! all sessions for that repo afterwards.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::process::Command;

/// All git invocations get this hermetic env so the dev user's
/// `~/.gitconfig` can't poison the bare-clone state. Mirrors the
/// `engram-host-agent::checkpoint::run_git` env — see that doc for
/// the rationale.
fn apply_hermetic_env(cmd: &mut Command) -> &mut Command {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
}

#[derive(Clone)]
pub struct GitWorkdir {
    root: PathBuf,
    locks: Arc<DashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    fetch_timeout: Duration,
}

impl GitWorkdir {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            locks: Arc::new(DashMap::new()),
            fetch_timeout: Duration::from_secs(60),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Return the bare-clone directory for `url`, creating it if
    /// missing and fetching from origin to bring it current. Per-URL
    /// serialisation makes concurrent calls for the same URL safe;
    /// concurrent calls for different URLs run in parallel.
    pub async fn ensure_clone(&self, url: &str) -> Result<PathBuf, GitWorkdirError> {
        let lock = self.lock_for(url);
        let _guard = lock.lock().await;
        let dir = self.dir_for(url);
        if dir.join("HEAD").exists() {
            // Already cloned — refresh refs.
            self.git_fetch(&dir).await?;
        } else {
            // First touch: clone bare. tokio::fs::create_dir_all is
            // idempotent; git clone refuses non-empty dirs, so we
            // create the parent only.
            if let Some(parent) = dir.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| GitWorkdirError::Io(format!("create root: {e}")))?;
            }
            self.git_clone_bare(url, &dir).await?;
        }
        Ok(dir)
    }

    /// Run `git -C <dir> <argv>` against the bare clone for `url`.
    /// Errors carry the captured stderr so callers can surface
    /// useful messages.
    pub async fn run_git(&self, url: &str, argv: &[&str]) -> Result<GitOutput, GitWorkdirError> {
        let dir = self.ensure_clone(url).await?;
        run_git_at(&dir, argv, self.fetch_timeout).await
    }

    /// Push `sha:branch` to `url`'s remote — used by `engram
    /// session fork` to publish a forked checkpoint branch.
    /// Pre-condition: the bare clone already has the SHA (in
    /// practice it does, because the original checkpoint was
    /// pushed there and our last fetch picked it up).
    pub async fn push_sha_to_branch(
        &self,
        url: &str,
        sha: &str,
        branch: &str,
    ) -> Result<(), GitWorkdirError> {
        let refspec = format!("{sha}:refs/heads/{branch}");
        let out = self.run_git(url, &["push", "origin", &refspec]).await?;
        if !out.exit_success() {
            return Err(GitWorkdirError::Git {
                stage: "push".into(),
                stderr: out.stderr_lossy(),
            });
        }
        Ok(())
    }

    fn dir_for(&self, url: &str) -> PathBuf {
        // sha256 keeps the dir name fixed-width and avoids filesystem
        // nightmares with `/`, `:`, `@` in URLs. Truncate to 16 hex
        // chars for readability — collisions on truncated sha256 are
        // not a concern at the per-deployment scale.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hasher::write(&mut hasher, url.as_bytes());
        let h = std::hash::Hasher::finish(&mut hasher);
        self.root.join(format!("{h:016x}.git"))
    }

    fn lock_for(&self, url: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .entry(url.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    async fn git_clone_bare(&self, url: &str, dir: &Path) -> Result<(), GitWorkdirError> {
        let mut cmd = Command::new("git");
        apply_hermetic_env(&mut cmd);
        cmd.arg("clone").arg("--bare").arg(url).arg(dir);
        let out = cmd
            .output()
            .await
            .map_err(|e| GitWorkdirError::Io(format!("git clone --bare: {e}")))?;
        if !out.status.success() {
            return Err(GitWorkdirError::Git {
                stage: "clone".into(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        Ok(())
    }

    async fn git_fetch(&self, dir: &Path) -> Result<(), GitWorkdirError> {
        // Explicit `+refs/heads/*:refs/heads/*` refspec because
        // `git clone --bare URL` doesn't always configure
        // `remote.origin.fetch` on macOS git, so a bare `git fetch
        // origin` ends up fetching only HEAD and stalls on new
        // branches like `engram/sessions/<id>`. The explicit
        // refspec works regardless of how clone was configured.
        let out = run_git_at(
            dir,
            &["fetch", "--prune", "origin", "+refs/heads/*:refs/heads/*"],
            self.fetch_timeout,
        )
        .await?;
        if !out.exit_success() {
            return Err(GitWorkdirError::Git {
                stage: "fetch".into(),
                stderr: out.stderr_lossy(),
            });
        }
        Ok(())
    }
}

/// Run `git -C <dir> <argv>` with the hermetic env and a wall-clock
/// timeout. Returns the captured `GitOutput` (stdout/stderr/exit)
/// regardless of success — callers decide how to handle non-zero
/// exit. Timeout maps to `GitWorkdirError::Timeout`.
pub async fn run_git_at(
    dir: &Path,
    argv: &[&str],
    timeout: Duration,
) -> Result<GitOutput, GitWorkdirError> {
    let mut cmd = Command::new("git");
    apply_hermetic_env(&mut cmd);
    cmd.arg("-C").arg(dir);
    for a in argv {
        cmd.arg(a);
    }
    let fut = cmd.output();
    let output = match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(GitWorkdirError::Io(format!("git -C: {e}"))),
        Err(_) => return Err(GitWorkdirError::Timeout(timeout)),
    };
    Ok(GitOutput {
        status: output.status.code(),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

#[derive(Debug, Clone)]
pub struct GitOutput {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl GitOutput {
    pub fn exit_success(&self) -> bool {
        self.status == Some(0)
    }
    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn stderr_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

#[derive(Debug)]
pub enum GitWorkdirError {
    Io(String),
    Timeout(Duration),
    Git { stage: String, stderr: String },
}

impl std::fmt::Display for GitWorkdirError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(m) => write!(f, "git workdir io: {m}"),
            Self::Timeout(d) => write!(f, "git workdir timed out after {d:?}"),
            Self::Git { stage, stderr } => write!(f, "git workdir {stage}: {}", stderr.trim()),
        }
    }
}

impl std::error::Error for GitWorkdirError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    fn run_host(args: &[&str], cwd: &Path) {
        let out = StdCommand::new(args[0])
            .args(&args[1..])
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "host {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn seeded_remote(branch: &str, marker: &str) -> TempDir {
        let remote = TempDir::new().unwrap();
        run_host(
            &["git", "init", "--bare", "--initial-branch=main"],
            remote.path(),
        );
        let work = TempDir::new().unwrap();
        run_host(&["git", "init", "--initial-branch=main"], work.path());
        run_host(
            &[
                "git",
                "remote",
                "add",
                "origin",
                &format!("{}", remote.path().display()),
            ],
            work.path(),
        );
        std::fs::write(work.path().join("README.md"), marker).unwrap();
        run_host(&["git", "add", "-A"], work.path());
        run_host(
            &[
                "git",
                "-c",
                "user.email=t@e.local",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "init",
            ],
            work.path(),
        );
        run_host(
            &["git", "push", "origin", &format!("HEAD:{branch}")],
            work.path(),
        );
        remote
    }

    #[tokio::test]
    async fn ensure_clone_first_call_clones_then_subsequent_call_fetches() {
        let remote = seeded_remote("main", "hello");
        let url = format!("{}", remote.path().display());

        let workdir_root = TempDir::new().unwrap();
        let wd = GitWorkdir::new(workdir_root.path().to_path_buf());

        // First call: clones the bare repo into the per-URL dir.
        let dir1 = wd.ensure_clone(&url).await.unwrap();
        assert!(dir1.join("HEAD").exists(), "bare clone produced HEAD");
        assert!(
            dir1.starts_with(workdir_root.path()),
            "clone dir lives under the workdir root"
        );

        // Push a new branch to the remote, then ensure_clone again.
        // The fetch path must pick up the new branch.
        let work = TempDir::new().unwrap();
        run_host(&["git", "clone", &url, "."], work.path());
        std::fs::write(work.path().join("note.txt"), "v2").unwrap();
        run_host(&["git", "add", "-A"], work.path());
        run_host(
            &[
                "git",
                "-c",
                "user.email=t@e.local",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "v2",
            ],
            work.path(),
        );
        run_host(
            &["git", "push", "origin", "HEAD:engram/sessions/abc"],
            work.path(),
        );

        // Confirm the remote has the new branch (test-setup
        // sanity check, not the thing we're testing).
        let remote_refs = StdCommand::new("git")
            .args(["-C"])
            .arg(remote.path())
            .args(["for-each-ref", "--format=%(refname)"])
            .output()
            .unwrap();
        let remote_refs_text = String::from_utf8_lossy(&remote_refs.stdout);
        assert!(
            remote_refs_text.contains("engram/sessions/abc"),
            "test-setup sanity: remote should have engram/sessions/abc; got:\n{remote_refs_text}"
        );

        let dir2 = wd.ensure_clone(&url).await.unwrap();
        assert_eq!(dir1, dir2, "per-URL dir is stable across calls");

        let out = wd
            .run_git(&url, &["rev-parse", "engram/sessions/abc"])
            .await
            .unwrap();
        let refs = wd
            .run_git(&url, &["for-each-ref", "--format=%(refname)"])
            .await
            .unwrap();
        assert!(
            out.exit_success(),
            "fetch should have brought the new branch in. \
             refs in bare clone:\n{}\n\
             rev-parse stderr: {}",
            refs.stdout_lossy(),
            out.stderr_lossy(),
        );
        assert!(!out.stdout.is_empty(), "rev-parse returned a SHA");
    }

    #[tokio::test]
    async fn dir_for_is_stable_per_url_and_distinct_across_urls() {
        let workdir_root = TempDir::new().unwrap();
        let wd = GitWorkdir::new(workdir_root.path().to_path_buf());
        let a1 = wd.dir_for("https://github.com/x/a.git");
        let a2 = wd.dir_for("https://github.com/x/a.git");
        let b = wd.dir_for("https://github.com/x/b.git");
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
    }

    #[tokio::test]
    async fn push_sha_to_branch_publishes_a_new_ref_on_the_remote() {
        // Used by `engram session fork`: take a SHA already on
        // engram/sessions/<src> and publish it under
        // engram/sessions/<dst>.
        let remote = seeded_remote("main", "v1");
        // Publish a second branch directly so the bare clone has
        // an extra commit reachable.
        let work = TempDir::new().unwrap();
        run_host(
            &["git", "clone", &format!("{}", remote.path().display()), "."],
            work.path(),
        );
        std::fs::write(work.path().join("forked.txt"), "from fork").unwrap();
        run_host(&["git", "add", "-A"], work.path());
        run_host(
            &[
                "git",
                "-c",
                "user.email=t@e.local",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "fork point",
            ],
            work.path(),
        );
        let sha_out = StdCommand::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(work.path())
            .output()
            .unwrap();
        let sha = String::from_utf8_lossy(&sha_out.stdout).trim().to_string();
        run_host(
            &["git", "push", "origin", "HEAD:engram/sessions/source"],
            work.path(),
        );

        let workdir_root = TempDir::new().unwrap();
        let wd = GitWorkdir::new(workdir_root.path().to_path_buf());
        let url = format!("{}", remote.path().display());
        wd.ensure_clone(&url).await.unwrap();

        wd.push_sha_to_branch(&url, &sha, "engram/sessions/forked")
            .await
            .expect("push should succeed");

        // The remote now has both engram/sessions/source and
        // engram/sessions/forked pointing at the same SHA.
        let bare_sha = StdCommand::new("git")
            .args(["-C"])
            .arg(remote.path())
            .args(["rev-parse", "engram/sessions/forked"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&bare_sha.stdout).trim(),
            sha,
            "forked branch must point at the published SHA"
        );
    }
}
