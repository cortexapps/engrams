use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct CoordinatorConfig {
    pub bind_addr: String,
    pub database_url: String,
    pub mode: RunMode,
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
    // ADR 0051: the ADR 0031 human-authentication config (the former
    // `engram-auth` AuthConfig) is removed along with the whole `engram-auth`
    // crate. The coordinator no longer resolves human identity — `auth_tokens`
    // (the deployment bearer) and `app_grpc_tokens` (the app-gRPC bearer) are
    // the only machine-caller credentials it knows about.
    /// Local address the harness-channel TCP listener binds to.
    /// Harnesses spawned via `start_agent` on the Process backend dial
    /// this from the same host. `127.0.0.1:0` (default) lets the OS
    /// pick a free port; the coordinator reads back the bound address
    /// and plumbs it into the agent's env at session-create time.
    pub harness_listen_addr: std::net::SocketAddr,
    /// Address the orchestrator-facing app gRPC server binds to
    /// (ADR 0051 §2.3). Serves Session/ShellRelay/Fleet/Image/Secret
    /// beside the axum API. Loopback by default — the orchestrator is
    /// the only intended caller and co-locates with the coordinator
    /// in dev.
    pub app_grpc_addr: std::net::SocketAddr,
    /// Bearer tokens accepted on the app gRPC surface (ADR 0051 §5).
    /// Machine identity for exactly one caller (the orchestrator) —
    /// deliberately a separate credential from `auth_tokens` above
    /// (different caller, different blast radius, independently
    /// rotatable). More than one entry only during rotation overlap.
    ///
    /// Unlike `auth_tokens`, empty does NOT mean "auth disabled":
    /// this surface fails closed — no configured tokens, every call
    /// answers `unauthenticated` (boot is unaffected).
    pub app_grpc_tokens: Vec<String>,
    /// App committer identity stamped into every session's launch env as
    /// git's native `GIT_COMMITTER_NAME`/`GIT_COMMITTER_EMAIL` overrides —
    /// in-session commits are *committed* by the deployment's app identity
    /// (authorship stays with the gitconfig `[user]` block: the human
    /// initiator when known, else this same identity via the in-guest
    /// fallback, never git's guessed `root@<host>`). For a GitHub App the
    /// convention is `<APP_ID>+<APP_SLUG>[bot]@users.noreply.github.com`;
    /// other forges have their own shapes, hence plain config. `None` =
    /// don't stamp (today's env untouched).
    pub git_committer_email: Option<String>,
    /// Display name paired with `git_committer_email` (ignored without it).
    pub git_committer_name: String,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:8080".into(),
            database_url: "postgres://engram:engram@localhost:5432/engram".into(),
            mode: RunMode::Coordinator,
            local_path: PathBuf::from("./var/engram"),
            sandbox_backend: SandboxBackendChoice::Firecracker,
            default_image_version: "warm-bootstrap".into(),
            // Empty = auth disabled. Production deployments populate
            // this from `ENGRAM_AUTH_TOKENS` (or a future secret-store
            // hookup) at startup.
            auth_tokens: Vec::new(),
            harness_listen_addr: "127.0.0.1:0"
                .parse()
                .expect("default harness_listen_addr must parse"),
            app_grpc_addr: "127.0.0.1:50061"
                .parse()
                .expect("default app_grpc_addr must parse"),
            // Empty = fail closed (every app-gRPC call rejected), NOT
            // auth-off. Populated from `ENGRAM_APP_GRPC_TOKENS`.
            app_grpc_tokens: Vec::new(),
            git_committer_email: None,
            git_committer_name: "engrams".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunMode {
    Coordinator,
    All,
}

impl RunMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "coordinator" => Ok(Self::Coordinator),
            "all" => Ok(Self::All),
            other => Err(format!("invalid mode: {other}")),
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
        for (s, expected) in [("coordinator", RunMode::Coordinator), ("all", RunMode::All)] {
            assert_eq!(RunMode::parse(s).unwrap(), expected);
        }
        // `host` served no purpose on this binary — `engram-host-agent` is
        // the real `--mode=host` — and used to parse successfully only to
        // be rejected a few lines later in `main`. Now an ordinary unknown
        // mode, alongside any other typo.
        let err = RunMode::parse("host").unwrap_err();
        assert!(err.contains("host"), "error must echo the bad input");
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
        assert_eq!(cfg.bind_addr, "0.0.0.0:8080");
        assert!(cfg.database_url.ends_with(":5432/engram"));
    }
}
