use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct HostAgentConfig {
    /// Where the SandboxBackend stores per-VM working directories,
    /// Firecracker sockets, and snapshot files.
    pub work_dir: PathBuf,
    /// Default warm-pool size per active repo.
    pub warm_pool_size: u32,
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
}

impl Default for HostAgentConfig {
    fn default() -> Self {
        Self {
            work_dir: PathBuf::from("./var/sandboxes"),
            warm_pool_size: 2,
            heartbeat_interval: Duration::from_secs(5),
            local_snapshot_cap_bytes: 500 * 1024 * 1024 * 1024, // 500 GiB
            coordinator_endpoint: None,
            coordinator_token: None,
        }
    }
}
