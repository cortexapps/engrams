use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct HostAgentConfig {
    /// Where the SandboxBackend stores per-VM working directories,
    /// Firecracker sockets, and snapshot files.
    pub work_dir: PathBuf,
    /// How often to send heartbeats to the coordinator.
    pub heartbeat_interval: Duration,
    /// Local snapshot disk cap, in bytes. Eviction kicks in beyond this.
    pub local_snapshot_cap_bytes: u64,
    /// Coordinator endpoint to dial via WebSocket. The dialer appends
    /// `/api/hosts/connect`. `None` keeps the agent in standalone-dev
    /// mode (no outbound connection; ctrl-c shuts down).
    pub coordinator_endpoint: Option<String>,
    /// Bearer token sent to the coordinator on the WS upgrade. Must
    /// match an entry in the coordinator's `ENGRAM_AUTH_TOKENS`. `None`
    /// is acceptable when the coordinator is in dev (auth disabled).
    pub coordinator_token: Option<String>,
    /// Local address the harness-channel TCP listener binds. Harnesses
    /// spawned by `SandboxBackend::create()` (via `SandboxSpec::agent`)
    /// dial this from the same host. `127.0.0.1:0` (the default) lets
    /// the OS pick a free port; the host-agent reads back the bound
    /// address and plumbs it into the agent's env. Override to a fixed
    /// port if a firewall or container network demands it.
    pub harness_listen_addr: std::net::SocketAddr,
    /// ADR 0013: bind address for the gRPC `HostService` server. The
    /// coord dials this from inside the VPC; usually `0.0.0.0:9101`
    /// on the host-agent pod so any coord pod can reach it. `None`
    /// skips the gRPC server entirely (mode=all, local dev with no coord).
    pub grpc_listen_addr: Option<std::net::SocketAddr>,
    /// ADR 0013: externally-routable address the coord uses to dial
    /// the gRPC server. Sent as `host_addr` in
    /// `POST /api/hosts/register`. Typically `http://<pod-ip>:9101`,
    /// injected by the chart on K8s. `None` skips registration (mode=all).
    pub grpc_advertise_addr: Option<String>,
    /// ADR 0045 C2: bind address for the post-copy page-server listener
    /// (`migrate_peer::PeerServer`, default `0.0.0.0:9102` in
    /// production). The DEST sandbox's uffd-handler dials it during a
    /// live migration; auth is the per-export token, so the listener is
    /// inert without an in-flight export. `None` disables it (mode=all
    /// dev, non-Linux).
    pub migrate_peer_listen_addr: Option<std::net::SocketAddr>,
    /// ADR 0068: `"firecracker" | "vz" | "process"` — resolved ONCE in
    /// `main.rs` from the same `BackendChoice` arm that picks the
    /// `SandboxBackend` (decision 11: detection stays above the
    /// cfg-gated leaves, never re-derived deeper in the stack). Feeds
    /// `capabilities::ProbeInputs::backend`. Defaults to `"process"` —
    /// the mode=all / in-process test-harness backend, which never
    /// constructs a `HostAgentConfig` through `main.rs`'s match anyway.
    pub backend_name: String,
}

impl Default for HostAgentConfig {
    fn default() -> Self {
        Self {
            work_dir: PathBuf::from("./var/sandboxes"),
            heartbeat_interval: Duration::from_secs(5),
            local_snapshot_cap_bytes: 500 * 1024 * 1024 * 1024, // 500 GiB
            coordinator_endpoint: None,
            coordinator_token: None,
            harness_listen_addr: "127.0.0.1:0"
                .parse()
                .expect("default harness_listen_addr must parse"),
            grpc_listen_addr: None,
            grpc_advertise_addr: None,
            migrate_peer_listen_addr: None,
            backend_name: "process".to_string(),
        }
    }
}
