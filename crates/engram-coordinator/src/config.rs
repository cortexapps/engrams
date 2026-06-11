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
    /// Bearer tokens accepted on protected endpoints. Empty = auth
    /// disabled (dev mode). When non-empty, every request to anything
    /// other than `/healthz` must carry `Authorization: Bearer <t>`
    /// where `<t>` is in this list.
    ///
    /// Stored as plaintext in memory — the production deployment is
    /// expected to source tokens from a secret manager and rotate
    /// the process; for v1 we don't hot-reload.
    pub auth_tokens: Vec<String>,
    /// ADR 0031 human authentication config (OIDC / forward-auth / synthetic).
    /// `AuthMode::None` (the default) → synthetic admin, so `just dev` and the
    /// test harness run with zero auth setup. `auth_tokens` above is folded
    /// into `auth.service_tokens` at startup for the machine-caller path.
    pub auth: engram_auth::AuthConfig,
    /// Local address the harness-channel TCP listener binds to.
    /// Harnesses spawned via `start_agent` on the Process backend dial
    /// this from the same host. `127.0.0.1:0` (default) lets the OS
    /// pick a free port; the coordinator reads back the bound address
    /// and plumbs it into the agent's env at session-create time.
    pub harness_listen_addr: std::net::SocketAddr,
    /// Address the orchestrator-facing app gRPC server binds to
    /// (ADR 0039 §2.3). Serves Session/ShellRelay/Fleet/Image/Secret
    /// beside the axum API during the migration; the axum web routes
    /// retire in Phase 5. Loopback by default — the orchestrator is
    /// the only intended caller and co-locates with the coordinator
    /// in dev.
    pub app_grpc_addr: std::net::SocketAddr,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:8080".into(),
            database_url: "postgres://engram:engram@localhost:5432/engram".into(),
            mode: RunMode::Coordinator,
            cloud_backend: CloudBackendChoice::Static,
            local_path: PathBuf::from("./var/engram"),
            sandbox_backend: SandboxBackendChoice::Firecracker,
            default_image_version: "warm-bootstrap".into(),
            // Empty = auth disabled. Production deployments populate
            // this from `ENGRAM_AUTH_TOKENS` (or a future secret-store
            // hookup) at startup.
            auth_tokens: Vec::new(),
            auth: engram_auth::AuthConfig::default(),
            harness_listen_addr: "127.0.0.1:0"
                .parse()
                .expect("default harness_listen_addr must parse"),
            app_grpc_addr: "127.0.0.1:50061"
                .parse()
                .expect("default app_grpc_addr must parse"),
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
    /// **Dev only (ADR 0023).** Un-isolated host subprocesses
    /// (`engram-sandbox-process`). Lets the product plane (coord API,
    /// integrations) run without KVM — on a laptop or inside an engrams
    /// session. Hard-gated in `main.rs` behind
    /// `ENGRAM_ALLOW_INSECURE_PROCESS_BACKEND=1`; never serves untrusted
    /// input.
    Process,
}

impl SandboxBackendChoice {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "firecracker" => Ok(Self::Firecracker),
            "vz" => Ok(Self::Vz),
            "process" => Ok(Self::Process),
            other => Err(format!(
                "invalid sandbox backend `{other}` — expected `firecracker`, `vz`, or `process`"
            )),
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
        // ADR 0023 re-introduced `process` as an operator-selectable
        // dev backend (it had been removed when harness dispatch moved
        // onto the SandboxBackend trait). It's back so the product plane
        // can run without KVM; the insecure-flag gate lives in main.rs,
        // not in this parse.
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
