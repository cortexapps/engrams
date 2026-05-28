//! Host egress-proxy CA cert installer (ADR 0021 P1.1).
//!
//! Replaces the pre-0021 delivery path where the host stamped the
//! per-host CA into the harness drive at `.engram-host/ca.pem` and
//! [`harness_supervisor::inject_egress_proxy_ca`] re-read it on every
//! spawn. With the harness baked into the rootfs (ADR 0021), the drive
//! goes away, so the host has to push the cert via vsock RPC instead.
//!
//! Mirrors E2B's `cacerts.go` install model (saved as
//! `reference_e2b_ca_cert_pattern` in user memory). Two write targets:
//!
//! - **`bundle`** — the file TLS libraries actually read, typically
//!   `/etc/ssl/certs/ca-certificates.crt`. We append the new cert here
//!   in the foreground so it takes effect immediately.
//! - **`extra_cert`** — the source-of-truth file
//!   `update-ca-certificates` regenerates the bundle from, typically
//!   `/usr/local/share/ca-certificates/engram-egress-proxy-ca.crt`.
//!   Persisted right after the bundle append so a later
//!   `update-ca-certificates` (run by anything in the rootfs that
//!   touches CAs) still includes our cert.
//!
//! An in-process `last_pem` cache makes the resume-with-same-cert path
//! zero-I/O — sessions that move across hosts get a fresh PEM and pay
//! one write; sessions that resume on the same host pay nothing.
//!
//! Unlike E2B we **do not** background the extra-cert persist + bundle
//! cleanup. The workload is a few KB and rotation isn't on the
//! latency-critical boot path; keeping everything synchronous makes
//! tests simpler and removes a class of "background goroutine still
//! running on agentd shutdown" bugs. If profiling later shows the
//! install dominates the boot path we can lift the second-write into
//! `tokio::task::spawn_blocking` cheaply.

use std::io;
use std::path::{Path, PathBuf};

use tokio::sync::Mutex;

/// Filesystem paths the installer writes to. Test code supplies
/// per-test temp paths; production uses [`CaCertPaths::default_linux`].
#[derive(Clone, Debug)]
pub struct CaCertPaths {
    /// The bundle TLS libraries read. PEM-formatted, append-friendly.
    pub bundle: PathBuf,
    /// The source-of-truth file `update-ca-certificates` regenerates
    /// the bundle from. One PEM per file.
    pub extra_cert: PathBuf,
}

impl CaCertPaths {
    /// Debian/Ubuntu-flavoured layout. Matches every base image
    /// engram ships and every published built-in harness so far.
    pub fn default_linux() -> Self {
        Self {
            bundle: PathBuf::from("/etc/ssl/certs/ca-certificates.crt"),
            extra_cert: PathBuf::from(
                "/usr/local/share/ca-certificates/engram-egress-proxy-ca.crt",
            ),
        }
    }
}

/// Installs the per-host CA cert into the guest's TLS trust store.
///
/// Idempotent: a resume with the same PEM as last time is a zero-I/O
/// hot path. A rotation (different PEM) does a fresh append + persist
/// + cleanup of the previous cert from the bundle.
pub struct CaCertInstaller {
    paths: CaCertPaths,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Most-recently installed PEM (normalised — single trailing
    /// newline). `None` before the first install; lost on agentd
    /// restart, which is acceptable — the stale cert (if any) stays
    /// in the bundle until the next rotation removes it, and the
    /// extra-cert file always carries the current cert.
    last_pem: Option<String>,
}

impl CaCertInstaller {
    pub fn new(paths: CaCertPaths) -> Self {
        Self {
            paths,
            state: Mutex::new(State::default()),
        }
    }

    /// Construct an installer with throwaway paths under
    /// `std::env::temp_dir()`. Used by tests that thread an installer
    /// into [`crate::handler::serve_connection`] but don't actually
    /// exercise [`Self::install`] — saves them from constructing a
    /// `CaCertPaths` themselves, and keeps any *accidental* install
    /// off `/etc/ssl/certs/` on a dev machine.
    pub fn for_tests() -> Self {
        // Unique-enough per process; collisions would just share a
        // bundle, which is fine — these installers are never the
        // target of a real cert exchange.
        let stem = format!("engram-agentd-cacerts-test-{}", std::process::id(),);
        let root = std::env::temp_dir().join(stem);
        Self::new(CaCertPaths {
            bundle: root.join("etc/ssl/certs/ca-certificates.crt"),
            extra_cert: root.join("usr/local/share/ca-certificates/engram.crt"),
        })
    }

    /// Install `pem` into the trust store. Returns `Ok(true)` when
    /// new bytes were written (first install or rotation),
    /// `Ok(false)` when the PEM matched the cached install (zero-I/O
    /// resume) or was empty.
    pub async fn install(&self, pem: &str) -> io::Result<bool> {
        if pem.is_empty() {
            return Ok(false);
        }
        let normalized = normalize_pem(pem);

        let mut state = self.state.lock().await;
        if state.last_pem.as_deref() == Some(normalized.as_str()) {
            return Ok(false);
        }
        let prev = state.last_pem.take();

        append_to_bundle(&self.paths.bundle, &normalized)?;
        write_extra_cert(&self.paths.extra_cert, &normalized)?;

        if let Some(prev_pem) = prev {
            // Best-effort: a failed rewrite leaves the *previous*
            // cert in the bundle, not a security regression — the
            // new cert is already installed and trusted. Log + move
            // on rather than failing the RPC.
            if let Err(e) = remove_from_bundle(&self.paths.bundle, &prev_pem) {
                tracing::warn!(
                    error = %e,
                    bundle = %self.paths.bundle.display(),
                    "failed to remove previous CA cert from bundle; new cert is still installed",
                );
            }
        }

        state.last_pem = Some(normalized);
        Ok(true)
    }
}

/// Trim trailing whitespace and append a single newline so the bundle
/// stays parseable regardless of how the caller formatted the PEM.
fn normalize_pem(pem: &str) -> String {
    let mut out = pem.trim_end().to_string();
    out.push('\n');
    out
}

/// Append `pem` to `bundle`, ensuring there's a newline separator if
/// the existing file doesn't already end with one. Creates the file
/// (and any missing parent directories) if absent.
fn append_to_bundle(bundle: &Path, pem: &str) -> io::Result<()> {
    if let Some(parent) = bundle.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut existing = std::fs::read(bundle).unwrap_or_default();
    if !existing.is_empty() && !existing.ends_with(b"\n") {
        existing.push(b'\n');
    }
    existing.extend_from_slice(pem.as_bytes());
    std::fs::write(bundle, &existing)
}

/// Atomically write `pem` to `path` (temp file in same dir + rename),
/// creating parent directories if needed.
fn write_extra_cert(path: &Path, pem: &str) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("extra-cert path has no parent: {}", path.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("engram-ca")
    ));
    std::fs::write(&tmp, pem.as_bytes())?;
    std::fs::rename(&tmp, path)
}

/// Rewrite `bundle` with every literal occurrence of `pem` stripped.
/// Atomic via temp-file + rename in the same directory so concurrent
/// readers always see a fully-formed bundle. No-op if `pem` doesn't
/// appear in the file.
fn remove_from_bundle(bundle: &Path, pem: &str) -> io::Result<()> {
    let content = std::fs::read_to_string(bundle)?;
    if !content.contains(pem) {
        return Ok(());
    }
    let cleaned = content.replace(pem, "");
    let parent = bundle.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("bundle path has no parent: {}", bundle.display()),
        )
    })?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        bundle
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("ca-bundle")
    ));
    std::fs::write(&tmp, cleaned.as_bytes())?;
    std::fs::rename(&tmp, bundle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, CaCertInstaller) {
        let tmp = TempDir::new().unwrap();
        let paths = CaCertPaths {
            bundle: tmp.path().join("etc/ssl/certs/ca-certificates.crt"),
            extra_cert: tmp
                .path()
                .join("usr/local/share/ca-certificates/engram.crt"),
        };
        let inst = CaCertInstaller::new(paths);
        (tmp, inst)
    }

    const PEM_A: &str = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----";
    const PEM_B: &str = "-----BEGIN CERTIFICATE-----\nBBBB\n-----END CERTIFICATE-----";

    #[tokio::test]
    async fn install_creates_bundle_and_extra_cert() {
        let (_tmp, inst) = fixture();
        assert!(inst.install(PEM_A).await.unwrap(), "first install changed");

        let bundle = std::fs::read_to_string(&inst.paths.bundle).unwrap();
        assert!(bundle.contains("AAAA"));
        let extra = std::fs::read_to_string(&inst.paths.extra_cert).unwrap();
        assert!(extra.contains("AAAA"));
        // Trailing newline normalisation.
        assert!(bundle.ends_with('\n'));
    }

    #[tokio::test]
    async fn empty_pem_is_a_noop() {
        let (_tmp, inst) = fixture();
        assert!(
            !inst.install("").await.unwrap(),
            "empty PEM must not change anything"
        );
        assert!(!inst.paths.bundle.exists(), "no bundle should be written");
    }

    #[tokio::test]
    async fn same_pem_is_zero_io_resume() {
        let (_tmp, inst) = fixture();
        assert!(inst.install(PEM_A).await.unwrap());
        let before = std::fs::metadata(&inst.paths.bundle)
            .unwrap()
            .modified()
            .unwrap();
        // Sleep so an mtime change would be visible on coarse-resolution FSes.
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!inst.install(PEM_A).await.unwrap(), "no rewrite expected");
        let after = std::fs::metadata(&inst.paths.bundle)
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(before, after, "bundle file must not be touched on no-op");
    }

    #[tokio::test]
    async fn rotation_replaces_previous_cert_in_bundle() {
        let (_tmp, inst) = fixture();
        assert!(inst.install(PEM_A).await.unwrap());
        assert!(inst.install(PEM_B).await.unwrap(), "rotation = changed");

        let bundle = std::fs::read_to_string(&inst.paths.bundle).unwrap();
        assert!(bundle.contains("BBBB"));
        assert!(
            !bundle.contains("AAAA"),
            "previous cert must be stripped from the bundle on rotation"
        );
        // extra_cert is overwritten to the *current* cert.
        let extra = std::fs::read_to_string(&inst.paths.extra_cert).unwrap();
        assert!(extra.contains("BBBB"));
        assert!(!extra.contains("AAAA"));
    }

    #[tokio::test]
    async fn pem_with_or_without_trailing_newline_compares_equal() {
        let (_tmp, inst) = fixture();
        assert!(inst.install(PEM_A).await.unwrap());
        // Caller normalises differently — must still be a no-op.
        let with_nl = format!("{PEM_A}\n\n");
        assert!(
            !inst.install(&with_nl).await.unwrap(),
            "normalisation strips trailing whitespace before compare"
        );
    }

    #[tokio::test]
    async fn append_preserves_existing_certs() {
        let (_tmp, inst) = fixture();
        // Pre-seed the bundle with some existing CA content (no trailing newline).
        std::fs::create_dir_all(inst.paths.bundle.parent().unwrap()).unwrap();
        std::fs::write(&inst.paths.bundle, b"-----DIFFERENT CERT-----").unwrap();

        assert!(inst.install(PEM_A).await.unwrap());
        let bundle = std::fs::read_to_string(&inst.paths.bundle).unwrap();
        assert!(bundle.contains("DIFFERENT CERT"));
        assert!(bundle.contains("AAAA"));
    }
}
