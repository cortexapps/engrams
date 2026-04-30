use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct CoordinatorConfig {
    pub bind_addr: String,
    pub database_url: String,
    pub mode: RunMode,
    pub cloud_backend: CloudBackendChoice,
    /// Local-disk root for snapshot dirs and the image registry.
    /// Per-host; not durable across host loss (cross-host durability
    /// for sessions is git, not local snapshots).
    pub local_path: PathBuf,
    pub sandbox_backend: SandboxBackendChoice,
    pub default_image_version: String,
    /// Target warm-pool size for any (repo, image_version) the
    /// coordinator has seen a session for. 0 disables the warm pool
    /// (every session creates a fresh sandbox synchronously).
    pub default_warm_pool_size: u32,
    /// Bearer tokens accepted on protected endpoints. Empty = auth
    /// disabled (dev mode). When non-empty, every request to anything
    /// other than `/healthz` must carry `Authorization: Bearer <t>`
    /// where `<t>` is in this list.
    ///
    /// Stored as plaintext in memory — the production deployment is
    /// expected to source tokens from a secret manager and rotate
    /// the process; for v1 we don't hot-reload.
    pub auth_tokens: Vec<String>,
    /// Local address the harness-channel TCP listener binds to.
    /// Harnesses spawned via `SandboxSpec::agent` dial this from the
    /// same host. `127.0.0.1:0` (default) lets the OS pick a free
    /// port; the coordinator reads back the bound address and
    /// plumbs it into the agent's env at session-create time.
    pub harness_listen_addr: std::net::SocketAddr,
    /// Path to the dev `engram-harness-noop` binary. When
    /// `dev_auto_agent = Some(Noop)`, the coordinator stamps this into
    /// `SandboxSpec::agent.argv[0]` for any new Git/Local session
    /// that doesn't already declare an agent. None disables
    /// auto-spawn even when `dev_auto_agent = Some(Noop)`.
    pub dev_noop_harness_path: Option<PathBuf>,
    /// Path to the dev `engram-harness-claude` binary. Symmetric to
    /// `dev_noop_harness_path`. For Firecracker, this is the in-rootfs
    /// path (typically `/sbin/engram-harness-claude`). For Process,
    /// it's a host path.
    pub dev_claude_harness_path: Option<PathBuf>,
    /// Auto-spawn a harness for every new session in dev. None = off.
    /// Set via `ENGRAM_DEV_AUTO_AGENT=noop|claude`. Implies the
    /// corresponding `dev_*_harness_path` is set.
    pub dev_auto_agent: Option<DevAgent>,
}

/// Which adapter the dev coordinator auto-spawns inside every new
/// sandbox. Each variant has a corresponding `dev_*_harness_path`
/// field that must be set for the auto-spawn to take effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DevAgent {
    /// `engram-harness-noop` — predictable fake tool calls, used for
    /// integration tests and as the default demo harness.
    Noop,
    /// `engram-harness-claude` — wraps the real `claude` CLI, requires
    /// `ANTHROPIC_API_KEY` in the operator's env.
    Claude,
}

impl DevAgent {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "noop" => Ok(Self::Noop),
            "claude" => Ok(Self::Claude),
            other => Err(format!("invalid dev agent: {other} (expected noop|claude)")),
        }
    }
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:8080".into(),
            database_url: "postgres://engram:engram@localhost:5432/engram".into(),
            mode: RunMode::Coordinator,
            cloud_backend: CloudBackendChoice::Static,
            local_path: PathBuf::from("./var/engram"),
            sandbox_backend: SandboxBackendChoice::Process,
            default_image_version: "warm-bootstrap".into(),
            // Modest dev default: one warm sandbox per (repo, image)
            // so the second session checkout is sub-second. Production
            // tunes this per-repo via the per-host-agent config.
            default_warm_pool_size: 1,
            // Empty = auth disabled. Production deployments populate
            // this from `ENGRAM_AUTH_TOKENS` (or a future secret-store
            // hookup) at startup.
            auth_tokens: Vec::new(),
            harness_listen_addr: "127.0.0.1:0"
                .parse()
                .expect("default harness_listen_addr must parse"),
            dev_noop_harness_path: None,
            dev_claude_harness_path: None,
            dev_auto_agent: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunMode {
    Coordinator,
    Host,
    All,
}

impl RunMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "coordinator" => Ok(Self::Coordinator),
            "host" => Ok(Self::Host),
            "all" => Ok(Self::All),
            other => Err(format!("invalid mode: {other}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloudBackendChoice {
    Static,
    Gcp,
    Mock,
}

impl CloudBackendChoice {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "static" => Ok(Self::Static),
            "gcp" => Ok(Self::Gcp),
            "mock" => Ok(Self::Mock),
            other => Err(format!("invalid cloud backend: {other}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxBackendChoice {
    /// Production on Linux: Firecracker microVMs (KVM-only). Real
    /// isolation, real resource enforcement, real snapshot/restore.
    Firecracker,
    /// Production on macOS Apple Silicon: Apple Virtualization.framework
    /// via the `engram-sandbox-vz` crate. Same wire surface as
    /// Firecracker (vsock UDS at `<work_dir>/<sid>.vsock_*`), full
    /// memory snapshot/restore via `saveMachineStateTo`. Coord binary
    /// must carry the `com.apple.security.virtualization` entitlement
    /// (see `just vz-codesign`).
    Vz,
    /// Local dev: plain host subprocesses, no isolation. Lets the
    /// orchestrator run end-to-end on any platform without a VMM.
    /// NEVER use in deployment.
    Process,
}

impl SandboxBackendChoice {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "firecracker" => Ok(Self::Firecracker),
            "vz" => Ok(Self::Vz),
            "process" => Ok(Self::Process),
            other => Err(format!("invalid sandbox backend: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_mode_parse_round_trip_and_rejects_unknown() {
        for (s, expected) in [
            ("coordinator", RunMode::Coordinator),
            ("host", RunMode::Host),
            ("all", RunMode::All),
        ] {
            assert_eq!(RunMode::parse(s).unwrap(), expected);
        }
        let err = RunMode::parse("worker").unwrap_err();
        assert!(err.contains("worker"), "error must echo the bad input");
    }

    #[test]
    fn run_mode_parse_is_case_sensitive() {
        // Document explicit behaviour: env-driven configuration is
        // case-sensitive. If we ever loosen this, this test should
        // change deliberately rather than as a side effect.
        assert!(RunMode::parse("Coordinator").is_err());
        assert!(RunMode::parse("ALL").is_err());
    }

    #[test]
    fn cloud_backend_parse_rejects_typos() {
        assert_eq!(
            CloudBackendChoice::parse("static").unwrap(),
            CloudBackendChoice::Static
        );
        assert_eq!(
            CloudBackendChoice::parse("gcp").unwrap(),
            CloudBackendChoice::Gcp
        );
        assert_eq!(
            CloudBackendChoice::parse("mock").unwrap(),
            CloudBackendChoice::Mock
        );
        assert!(CloudBackendChoice::parse("aws").is_err());
        assert!(CloudBackendChoice::parse("").is_err());
    }

    #[test]
    fn sandbox_backend_parse_rejects_unknown() {
        assert_eq!(
            SandboxBackendChoice::parse("firecracker").unwrap(),
            SandboxBackendChoice::Firecracker,
        );
        assert_eq!(
            SandboxBackendChoice::parse("vz").unwrap(),
            SandboxBackendChoice::Vz,
        );
        assert_eq!(
            SandboxBackendChoice::parse("process").unwrap(),
            SandboxBackendChoice::Process,
        );
        // microsandbox was a Phase 1 alternative; removed when we
        // committed to Firecracker for production.
        assert!(SandboxBackendChoice::parse("microsandbox").is_err());
        assert!(SandboxBackendChoice::parse("kata").is_err());
        assert!(SandboxBackendChoice::parse("vfkit").is_err());
    }

    #[test]
    fn default_config_uses_static_local_coordinator() {
        let cfg = CoordinatorConfig::default();
        assert_eq!(cfg.mode, RunMode::Coordinator);
        assert_eq!(cfg.cloud_backend, CloudBackendChoice::Static);
        assert_eq!(cfg.bind_addr, "0.0.0.0:8080");
        assert!(cfg.database_url.ends_with(":5432/engram"));
    }
}
