//! Thin abstraction over the `docker` CLI so unit tests can mock it.
//!
//! [`DockerCli`] is the production implementation that shells out via
//! `tokio::process::Command`. Tests use a recording mock implemented
//! against the [`DockerRunner`] trait.
//!
//! We deliberately don't depend on a Rust Docker SDK crate. Most are
//! either unmaintained, heavyweight, or both. The CLI is universal,
//! supports BuildKit out of the box, and has stable enough behaviour
//! that `Command`-shelling is the lowest-friction option for v1.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Clone, Debug)]
pub struct BuildArgs {
    pub context: PathBuf,
    pub dockerfile: PathBuf,
    pub tag: String,
    pub build_args: HashMap<String, String>,
    /// BuildKit build-time secret IDs (the `[build] build_secrets` names).
    /// Each becomes `--secret id=<name>,env=<name>`, so BuildKit reads the
    /// value from the bake process's environment variable of the same name
    /// and exposes it to the Dockerfile via `RUN --mount=type=secret,
    /// id=<name>` (readable at `/run/secrets/<name>`). The value never
    /// lands in a build-arg, an image layer, or the image history — the
    /// leak-free way to feed a private-registry token to `pnpm install` /
    /// `gradle assemble` during the bake. CI sets the env vars from its
    /// secret store before invoking the bake.
    pub build_secrets: Vec<String>,
}

/// The subset of a built image's container config the baker folds into
/// the rendered manifest: the Dockerfile's `ENV` (as the raw Docker
/// `KEY=VALUE` strings) and `WORKDIR`. Read via `docker inspect` so a
/// Dockerfile's environment + working directory reach the guest without
/// the author restating them in `engram.toml` — the platform doesn't
/// otherwise read the OCI image config.
#[derive(Clone, Debug, Default)]
pub struct DockerImageConfig {
    pub env: Vec<String>,
    pub working_dir: Option<String>,
}

/// Minimal interface the baker uses. Implementations:
///
/// - [`DockerCli`] — production: shells out to `docker`.
/// - Tests provide their own recording mock (see `tests/`).
#[async_trait]
pub trait DockerRunner: Send + Sync {
    async fn build(&self, args: BuildArgs) -> Result<(), DockerError>;

    /// Create a container from a built image. Returns the container
    /// id (the short or long hash — caller treats as opaque).
    async fn create(&self, image_tag: &str) -> Result<String, DockerError>;

    /// Export the container's filesystem and untar it into `dest`.
    /// `dest` must already exist.
    async fn export_to_dir(&self, container_id: &str, dest: &Path) -> Result<(), DockerError>;

    async fn rm_container(&self, container_id: &str) -> Result<(), DockerError>;

    async fn rmi(&self, image_tag: &str) -> Result<(), DockerError>;

    /// Reclaim the BuildKit build cache (`docker builder prune`). After a
    /// build the cache holds a full copy of the image's layers; `rmi` drops
    /// the image reference but NOT that cache, so for a large warm image the
    /// ~image-sized cache lingers on disk through the ext4 pack and can
    /// overrun the runner (`mke2fs: No space left on device`). The baker
    /// calls this between export and pack to drop the pack's disk peak to
    /// (exported tree + ext4). Best-effort — a prune failure must not fail
    /// the bake.
    async fn builder_prune(&self) -> Result<(), DockerError>;

    /// Read `Config.Env` + `Config.WorkingDir` off a created container
    /// (or image) via `docker inspect`. A created-but-unstarted
    /// container's `Config` mirrors the image's `ENV`/`WORKDIR`, so the
    /// baker inspects the container it already made for the export. The
    /// baker folds these into the rendered manifest as defaults under
    /// the author's `engram.toml`.
    async fn inspect_config(&self, id: &str) -> Result<DockerImageConfig, DockerError>;

    /// For the [`Drop`] cleanup helper in `Builder` to launch
    /// best-effort async cleanup tasks. Implementations return a
    /// boxed clone of themselves.
    fn clone_runner(&self) -> Box<dyn DockerRunner>;
}

#[derive(Debug)]
pub enum DockerError {
    NotFound,
    NonZeroExit {
        command: String,
        code: Option<i32>,
        stderr: String,
    },
    /// The command produced no output for the idle-timeout window and was
    /// killed as wedged. `tail` is the last output we saw before silence —
    /// the layer/step the build stalled on. This turns the historical
    /// silent hang (a `docker build` that stalls on a hung network fetch
    /// and rides the CI job to its wall-clock cap) into a loud, diagnosable
    /// failure.
    Timeout {
        command: String,
        idle_secs: u64,
        tail: String,
    },
    /// `docker` succeeded but its output didn't parse as expected (e.g.
    /// `docker inspect` JSON). Carries a human-readable reason.
    Parse(String),
    Io(std::io::Error),
}

impl std::fmt::Display for DockerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(
                f,
                "`docker` not on PATH (install Docker Desktop, OrbStack, Colima, or Podman with docker-compat)"
            ),
            Self::NonZeroExit { command, code, stderr } => write!(
                f,
                "`{command}` exited with {code:?}: {}",
                stderr.trim()
            ),
            Self::Timeout { command, idle_secs, tail } => write!(
                f,
                "`{command}` produced no output for {idle_secs}s and was killed as wedged. Last output before the stall:\n{}",
                tail.trim()
            ),
            Self::Parse(m) => write!(f, "docker output parse error: {m}"),
            Self::Io(e) => write!(f, "spawn: {e}"),
        }
    }
}

impl std::error::Error for DockerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for DockerError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::NotFound {
            Self::NotFound
        } else {
            Self::Io(e)
        }
    }
}

/// Production [`DockerRunner`]. Shells out to `docker`. Compatible
/// with Docker Desktop, OrbStack, Colima, and Podman (via
/// `podman-docker` shim).
#[derive(Clone, Debug, Default)]
pub struct DockerCli {
    /// Override the binary name. Default `docker`. Useful for users
    /// who run `podman` without the docker-compat alias.
    pub binary: Option<String>,
}

impl DockerCli {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            binary: Some(binary.into()),
        }
    }

    fn binary_str(&self) -> &str {
        self.binary.as_deref().unwrap_or("docker")
    }

    fn cmd(&self) -> Command {
        Command::new(self.binary_str())
    }
}

#[async_trait]
impl DockerRunner for DockerCli {
    async fn build(&self, args: BuildArgs) -> Result<(), DockerError> {
        let mut cmd = self.cmd();
        cmd.arg("build")
            .arg("-f")
            .arg(&args.dockerfile)
            .arg("-t")
            .arg(&args.tag);
        for (k, v) in &args.build_args {
            cmd.arg("--build-arg").arg(format!("{k}={v}"));
        }
        // BuildKit build-time secrets, sourced from the bake process env
        // (`env=<id>`). Requires the BuildKit frontend — force it on so the
        // `--secret` flag is honored even where the daemon defaults to the
        // legacy builder (which would silently ignore it).
        if !args.build_secrets.is_empty() {
            cmd.env("DOCKER_BUILDKIT", "1");
            for id in &args.build_secrets {
                cmd.arg("--secret").arg(format!("id={id},env={id}"));
            }
        }
        cmd.arg(&args.context);
        // Force BuildKit's line-oriented `plain` progress so the streamed
        // output is complete + parseable. The default `auto` switches to a
        // TTY renderer that collapses lines and won't stream usefully to a
        // pipe — which is exactly how a stalled build went *silent* in CI
        // (nothing reached the log) before this. Honored by BuildKit;
        // ignored by the legacy builder.
        cmd.env("BUILDKIT_PROGRESS", "plain");
        // Stream the build live (was buffered + discarded until exit) and
        // bound it with an idle-timeout so a wedged build fails loudly
        // instead of hanging to the CI job's wall-clock cap.
        run_streaming(cmd, "docker build", build_idle_timeout()).await
    }

    async fn create(&self, image_tag: &str) -> Result<String, DockerError> {
        let mut cmd = self.cmd();
        cmd.arg("create").arg(image_tag);
        let out = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;
        if !out.status.success() {
            return Err(DockerError::NonZeroExit {
                command: format!("docker create {image_tag}"),
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if id.is_empty() {
            return Err(DockerError::NonZeroExit {
                command: "docker create".into(),
                code: out.status.code(),
                stderr: "empty container id from docker create".into(),
            });
        }
        Ok(id)
    }

    async fn export_to_dir(&self, container_id: &str, dest: &Path) -> Result<(), DockerError> {
        // `docker export <id> | tar -x -C <dest>` — let the shell
        // handle the pipe so we don't have to plumb fds ourselves.
        // `set -o pipefail` so an export failure surfaces even if tar
        // happens to succeed first. Pinned to bash because Debian /
        // Ubuntu's `/bin/sh` is dash, which rejects `pipefail`
        // ("Illegal option -o pipefail"); bash is universally
        // available on Linux runners we ship to.
        let pipeline = format!(
            "set -e; set -o pipefail; {bin} export {cid} | tar -x -C {dest}",
            bin = shell_escape(self.binary_str()),
            cid = shell_escape(container_id),
            dest = shell_escape(&dest.to_string_lossy()),
        );
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(&pipeline);
        run_to_completion(cmd, "docker export | tar -x").await
    }

    async fn rm_container(&self, container_id: &str) -> Result<(), DockerError> {
        let mut cmd = self.cmd();
        cmd.arg("rm").arg("-f").arg(container_id);
        run_to_completion(cmd, "docker rm").await
    }

    async fn rmi(&self, image_tag: &str) -> Result<(), DockerError> {
        let mut cmd = self.cmd();
        cmd.arg("rmi").arg("-f").arg(image_tag);
        run_to_completion(cmd, "docker rmi").await
    }

    async fn builder_prune(&self) -> Result<(), DockerError> {
        // `-a` (all unused cache, not just dangling) + `-f` (no prompt). The
        // bake just exported the only image it built, so there is no cache
        // worth keeping for this run; CI runners start cache-cold anyway.
        let mut cmd = self.cmd();
        cmd.arg("builder").arg("prune").arg("-af");
        run_to_completion(cmd, "docker builder prune").await
    }

    async fn inspect_config(&self, id: &str) -> Result<DockerImageConfig, DockerError> {
        let mut cmd = self.cmd();
        cmd.arg("inspect")
            .arg("--format")
            .arg("{{json .Config}}")
            .arg(id);
        let out = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;
        if !out.status.success() {
            return Err(DockerError::NonZeroExit {
                command: format!("docker inspect {id}"),
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        parse_inspect_config(&out.stdout)
    }

    fn clone_runner(&self) -> Box<dyn DockerRunner> {
        Box::new(self.clone())
    }
}

/// Parse `docker inspect --format '{{json .Config}}'` output into the
/// env + workdir the baker folds. An empty `WorkingDir` (the value for
/// an image with no `WORKDIR`) maps to `None` so it doesn't shadow the
/// sandbox default. Split out from the CLI call so it's unit-testable
/// without a daemon.
fn parse_inspect_config(stdout: &[u8]) -> Result<DockerImageConfig, DockerError> {
    #[derive(serde::Deserialize)]
    struct InspectConfig {
        #[serde(rename = "Env", default)]
        env: Vec<String>,
        #[serde(rename = "WorkingDir", default)]
        working_dir: Option<String>,
    }
    let parsed: InspectConfig = serde_json::from_slice(stdout)
        .map_err(|e| DockerError::Parse(format!("inspect .Config: {e}")))?;
    Ok(DockerImageConfig {
        env: parsed.env,
        working_dir: parsed.working_dir.filter(|w| !w.is_empty()),
    })
}

/// Single-quote-escape a value for safe shell substitution. Docker
/// arguments and dest paths can contain spaces or shell metacharacters
/// from user-controlled inputs; this prevents injection.
fn shell_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            // Close, escape, reopen — the standard sh-quoting trick.
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// How many trailing lines of a streamed command's output to retain for
/// the error message. The *full* stream is echoed live to our own
/// stdout/stderr (so CI shows it as it happens); this bounded tail is
/// only what rides into a returned [`DockerError`].
const BUILD_TAIL_LINES: usize = 80;

/// Default idle (no-output) timeout for `docker build`. BuildKit streams
/// continuous progress, so a long *silence* — not total elapsed time — is
/// the reliable "wedged" signal (a hung network fetch inside a `RUN`, a
/// BuildKit stall). A genuinely long-but-progressing build keeps emitting
/// lines and never trips this; only a stall does. The observed prod hang
/// sat silent for ~24 min before hitting the job cap, so 10 min catches it
/// with margin. Override (or disable with `0`) via the env var below.
const DEFAULT_BUILD_IDLE_TIMEOUT_SECS: u64 = 600;

/// Env override for [`DEFAULT_BUILD_IDLE_TIMEOUT_SECS`]. `0` disables the
/// guard (unbounded). An unparseable value falls back to the default.
const BUILD_IDLE_TIMEOUT_ENV: &str = "ENGRAM_IMAGE_BUILD_IDLE_TIMEOUT_SECS";

/// Resolve the `docker build` idle timeout from the environment, falling
/// back to the default. `Some(d)` arms the guard; `None` disables it.
fn build_idle_timeout() -> Option<Duration> {
    match std::env::var(BUILD_IDLE_TIMEOUT_ENV) {
        Ok(v) => match v.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(n) => Some(Duration::from_secs(n)),
            Err(_) => Some(Duration::from_secs(DEFAULT_BUILD_IDLE_TIMEOUT_SECS)),
        },
        Err(_) => Some(Duration::from_secs(DEFAULT_BUILD_IDLE_TIMEOUT_SECS)),
    }
}

/// Run a child to completion while streaming its stdout+stderr live to our
/// own stdout/stderr (so the CI log shows progress in real time) and
/// retaining a bounded tail for diagnostics. If `idle_timeout` is set and
/// the child emits no output for that long, it's treated as wedged: the
/// child is killed and a [`DockerError::Timeout`] returned carrying the
/// last output before the stall.
///
/// Contrast with [`run_to_completion`], which uses `Command::output()` —
/// that buffers all output until the child *exits* and waits with no
/// timeout, so a stalled `docker build` produced zero CI output and hung
/// until the job's wall-clock cap. This streamer is the fix.
async fn run_streaming(
    mut cmd: Command,
    name: &str,
    idle_timeout: Option<Duration>,
) -> Result<(), DockerError> {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Backstop: if we bail (e.g. the caller is cancelled) without an
        // explicit kill, dropping the child still reaps it.
        .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("stdout piped above");
    let stderr = child.stderr.take().expect("stderr piped above");

    // Both pipes feed one channel; the bool marks a stderr line (BuildKit
    // writes its progress to stderr). Each reader task ends at pipe EOF —
    // which the child closing the pipe on exit guarantees — dropping its
    // sender, so the recv loop sees `None` once both have finished.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(bool, String)>(256);
    let tx_err = tx.clone();
    let out_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send((false, line)).await.is_err() {
                break;
            }
        }
    });
    let err_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx_err.send((true, line)).await.is_err() {
                break;
            }
        }
    });

    let mut tail: VecDeque<String> = VecDeque::with_capacity(BUILD_TAIL_LINES + 1);
    let stalled = loop {
        let item = match idle_timeout {
            // The wait for the *next* line is the silence window — a line
            // arriving resets it. `Err` means no line within `d`: wedged.
            Some(d) => match timeout(d, rx.recv()).await {
                Ok(item) => item,
                Err(_) => break true,
            },
            None => rx.recv().await,
        };
        match item {
            Some((is_err, line)) => {
                if is_err {
                    eprintln!("{line}");
                } else {
                    println!("{line}");
                }
                if tail.len() >= BUILD_TAIL_LINES {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
            // Both pipes hit EOF — the child has closed them, so it's
            // exiting; reap it below for the status.
            None => break false,
        }
    };

    if stalled {
        let _ = child.start_kill();
        let _ = child.wait().await;
        out_task.abort();
        err_task.abort();
        return Err(DockerError::Timeout {
            command: name.into(),
            idle_secs: idle_timeout.map(|d| d.as_secs()).unwrap_or(0),
            tail: tail.into_iter().collect::<Vec<_>>().join("\n"),
        });
    }

    let status = child.wait().await?;
    let _ = out_task.await;
    let _ = err_task.await;
    if !status.success() {
        return Err(DockerError::NonZeroExit {
            command: name.into(),
            code: status.code(),
            stderr: tail.into_iter().collect::<Vec<_>>().join("\n"),
        });
    }
    Ok(())
}

async fn run_to_completion(mut cmd: Command, name: &str) -> Result<(), DockerError> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = cmd.output().await?;
    if !out.status.success() {
        return Err(DockerError::NonZeroExit {
            command: name.into(),
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_escape_handles_simple_strings() {
        assert_eq!(shell_escape("docker"), "'docker'");
        assert_eq!(shell_escape("/tmp/a b"), "'/tmp/a b'");
    }

    #[test]
    fn shell_escape_handles_embedded_single_quotes() {
        // The classic injection vector: an arg containing `'`. Should
        // be split into close-escape-reopen so the shell still sees a
        // single literal token.
        assert_eq!(shell_escape("a'b"), "'a'\\''b'");
    }

    #[test]
    fn parse_inspect_config_extracts_env_and_workdir() {
        let json = br#"{"Env":["PATH=/opt/cargo/bin:/usr/bin","CARGO_HOME=/opt/cargo"],"WorkingDir":"/workspace","Cmd":["/bin/sh"]}"#;
        let cfg = parse_inspect_config(json).unwrap();
        assert_eq!(
            cfg.env,
            vec!["PATH=/opt/cargo/bin:/usr/bin", "CARGO_HOME=/opt/cargo"]
        );
        assert_eq!(cfg.working_dir.as_deref(), Some("/workspace"));
    }

    #[tokio::test]
    async fn run_streaming_ok_on_quick_command() {
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg("echo to-stdout; echo to-stderr 1>&2");
        let r = run_streaming(cmd, "test", Some(Duration::from_secs(10))).await;
        assert!(r.is_ok(), "expected success, got {r:?}");
    }

    #[tokio::test]
    async fn run_streaming_reports_nonzero_exit_with_tail() {
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg("echo boom 1>&2; exit 3");
        match run_streaming(cmd, "test", Some(Duration::from_secs(10))).await {
            Err(DockerError::NonZeroExit { code, stderr, .. }) => {
                assert_eq!(code, Some(3));
                assert!(
                    stderr.contains("boom"),
                    "tail should carry stderr: {stderr}"
                );
            }
            other => panic!("expected NonZeroExit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_streaming_times_out_on_a_silent_stall() {
        // Emit one line, then go silent well past the idle window — the
        // exact signature of the prod hang. The guard must fire and carry
        // the last line we saw.
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg("echo starting-step; sleep 30");
        match run_streaming(cmd, "test", Some(Duration::from_millis(300))).await {
            Err(DockerError::Timeout {
                tail, idle_secs, ..
            }) => {
                assert_eq!(idle_secs, 0, "sub-second idle window rounds to 0s");
                assert!(
                    tail.contains("starting-step"),
                    "tail should carry last output: {tail}"
                );
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_streaming_disabled_timeout_lets_a_brief_silence_pass() {
        // `None` disables the guard — a short silence must not be a stall.
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg("echo a; sleep 0.2; echo b");
        let r = run_streaming(cmd, "test", None).await;
        assert!(r.is_ok(), "expected success with guard disabled, got {r:?}");
    }

    #[test]
    fn build_idle_timeout_parses_env_overrides() {
        // Defaults when unset; `0` disables; a number arms; junk → default.
        let key = BUILD_IDLE_TIMEOUT_ENV;
        std::env::remove_var(key);
        assert_eq!(
            build_idle_timeout(),
            Some(Duration::from_secs(DEFAULT_BUILD_IDLE_TIMEOUT_SECS))
        );
        std::env::set_var(key, "0");
        assert_eq!(build_idle_timeout(), None);
        std::env::set_var(key, "42");
        assert_eq!(build_idle_timeout(), Some(Duration::from_secs(42)));
        std::env::set_var(key, "not-a-number");
        assert_eq!(
            build_idle_timeout(),
            Some(Duration::from_secs(DEFAULT_BUILD_IDLE_TIMEOUT_SECS))
        );
        std::env::remove_var(key);
    }

    #[test]
    fn parse_inspect_config_empty_workdir_is_none() {
        // Images without a WORKDIR report `"WorkingDir":""` — must not
        // shadow the sandbox default.
        let cfg = parse_inspect_config(br#"{"Env":[],"WorkingDir":""}"#).unwrap();
        assert!(cfg.env.is_empty());
        assert_eq!(cfg.working_dir, None);
        // Missing keys default cleanly too.
        let cfg = parse_inspect_config(br#"{}"#).unwrap();
        assert!(cfg.env.is_empty());
        assert_eq!(cfg.working_dir, None);
    }
}
