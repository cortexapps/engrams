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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;

#[derive(Clone, Debug)]
pub struct BuildArgs {
    pub context: PathBuf,
    pub dockerfile: PathBuf,
    pub tag: String,
    pub build_args: HashMap<String, String>,
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
        cmd.arg(&args.context);
        run_to_completion(cmd, "docker build").await
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
        // happens to succeed first.
        let pipeline = format!(
            "set -e; set -o pipefail; {bin} export {cid} | tar -x -C {dest}",
            bin = shell_escape(self.binary_str()),
            cid = shell_escape(container_id),
            dest = shell_escape(&dest.to_string_lossy()),
        );
        let mut cmd = Command::new("sh");
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

    fn clone_runner(&self) -> Box<dyn DockerRunner> {
        Box::new(self.clone())
    }
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
}
