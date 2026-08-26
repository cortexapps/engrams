//! ADR 0121: ensure, adopt, drive, and supervise the node-local
//! egress daemon (`engram-egress-proxyd`).
//!
//! The daemon owns the egress listeners and outlives this pod; the
//! host-agent is its control plane. `ensure_proxyd` gathers evidence
//! (manifest, `/proc` identity, a `Hello` round-trip, the accept-loop
//! probe) and executes the pure [`decide_adopt`] plan: adopt the
//! survivor, gracefully replace an old build, kill a wedged one, or
//! spawn fresh. Fail-closed throughout — the caller aborts host-agent
//! startup unless a serving daemon is confirmed (the ADR 0083
//! invariant, translated).
//!
//! Supervision is event-driven, not a scanner: the spawned child's
//! `wait()` (or the adopted pid's pidfd readability) IS the exit
//! event; on it, respawn + replay. No polling loop exists here.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use engram_core::{HostId, SessionId};
use engram_egress_proto::adopt::{decide_adopt, AdoptPlan, ExpectedProxyd, LiveEvidence};
use engram_egress_proto::manifest::{read_manifest, read_proc_identity, remove_manifest};
use engram_egress_proto::{FromProxyd, HelloInfo, ToProxyd};

/// The host-agent's own build-time fingerprint of the daemon's source
/// closure. Must use the same literal as the daemon's `option_env!`.
const HOST_SIDE_FINGERPRINT: Option<&str> = option_env!("ENGRAM_EGRESS_PROXYD_FINGERPRINT");

/// One `Hello`/probe attempt's budget against a possibly-wedged peer.
const HELLO_BUDGET: Duration = Duration::from_secs(3);
/// The accept-loop probe's close deadline.
const PROBE_BUDGET: Duration = Duration::from_secs(3);
/// Fresh-spawn readiness budget: covers the daemon's own
/// `bind_with_retry` (5×500 ms) with room for CA parse + cgroup I/O.
const SPAWN_READY_BUDGET: Duration = Duration::from_secs(20);
/// How long a graceful `Shutdown` (or a SIGKILL) gets to make the old
/// process disappear before we escalate / give up.
const EXIT_BUDGET: Duration = Duration::from_secs(10);

/// Everything needed to spawn (or respawn) the daemon.
#[derive(Clone)]
pub struct ProxydSpawnConfig {
    /// The daemon binary. Production: `ENGRAM_EGRESS_PROXYD_BIN`
    /// (`/usr/local/bin/engram-egress-proxyd` in the image); dev: the
    /// cargo-built sibling.
    pub bin: PathBuf,
    pub work_dir: PathBuf,
    pub proxy_port: u16,
    pub dns_port: u16,
    pub gateway_port: u16,
    /// `<ENGRAM_FC_VM_CGROUP_PARENT>/egress-proxyd` when the node
    /// cgroup escape is configured; `None` in dev.
    pub cgroup_dir: Option<PathBuf>,
    pub ca_cert_pem: String,
    pub ca_key_pem: String,
    pub coord_url: String,
    pub coord_token: Option<String>,
    pub host_id: HostId,
}

impl ProxydSpawnConfig {
    fn control_sock(&self) -> PathBuf {
        self.work_dir.join(engram_egress_proto::CONTROL_SOCK_NAME)
    }

    fn expected(&self) -> ExpectedProxyd<'_> {
        ExpectedProxyd {
            source_fingerprint: HOST_SIDE_FINGERPRINT,
            proxy_port: self.proxy_port,
            dns_port: self.dns_port,
            gateway_port: self.gateway_port,
            ca_fingerprint: "", // filled per call — lifetime helper below
            coord_url: &self.coord_url,
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The live daemon process the supervisor watches.
pub enum ProxydProcess {
    /// We spawned it this generation: `wait()` is the exit event.
    Spawned(tokio::process::Child),
    /// Adopted from a prior generation: the pidfd's readability is the
    /// exit event (Linux; adoption never happens elsewhere because
    /// local builds carry no fingerprint).
    Adopted(engram_sandbox_firecracker::pidfd::PidFd),
}

/// The control-socket client. Holds one cached connection and
/// reconnects on demand — a respawned daemon binds the same socket
/// path, so the handle survives daemon replacement unchanged.
pub struct ProxydHandle {
    control_sock: PathBuf,
    conn: tokio::sync::Mutex<Option<tokio::net::UnixStream>>,
}

#[derive(Debug)]
pub struct ProxydError(pub String);

impl std::fmt::Display for ProxydError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "egress proxyd: {}", self.0)
    }
}

impl std::error::Error for ProxydError {}

impl ProxydHandle {
    pub fn new(control_sock: PathBuf) -> Self {
        Self {
            control_sock,
            conn: tokio::sync::Mutex::new(None),
        }
    }

    /// One request/response round-trip. Serialized behind the mutex —
    /// the control plane is low-rate (policy applies and removals).
    /// On any transport error the cached connection is dropped so the
    /// next call redials (the respawn recovery path).
    async fn request(&self, msg: &ToProxyd) -> Result<FromProxyd, ProxydError> {
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            let stream = tokio::net::UnixStream::connect(&self.control_sock)
                .await
                .map_err(|e| {
                    ProxydError(format!("connect {}: {e}", self.control_sock.display()))
                })?;
            *guard = Some(stream);
        }
        let stream = guard.as_mut().expect("connection just ensured");
        let result: Result<FromProxyd, std::io::Error> = async {
            engram_egress_proto::write_frame(stream, msg).await?;
            engram_egress_proto::read_frame(stream).await
        }
        .await;
        match result {
            Ok(reply) => Ok(reply),
            Err(e) => {
                *guard = None;
                Err(ProxydError(format!("control round-trip: {e}")))
            }
        }
    }

    async fn expect_ok(&self, msg: &ToProxyd, what: &str) -> Result<(), ProxydError> {
        match self.request(msg).await? {
            FromProxyd::Ok => Ok(()),
            FromProxyd::Err(e) => Err(ProxydError(format!("{what}: {e}"))),
            other => Err(ProxydError(format!("{what}: unexpected reply {other:?}"))),
        }
    }

    pub async fn hello(&self) -> Result<HelloInfo, ProxydError> {
        match self
            .request(&ToProxyd::Hello {
                proto_version: engram_egress_proto::PROTO_VERSION,
            })
            .await?
        {
            FromProxyd::HelloAck(info) => Ok(info),
            other => Err(ProxydError(format!("hello: unexpected reply {other:?}"))),
        }
    }

    pub async fn apply_policy(
        &self,
        policy: engram_core::types::egress::SessionEgressPolicy,
    ) -> Result<(), ProxydError> {
        self.expect_ok(&ToProxyd::ApplyPolicy(Box::new(policy)), "apply policy")
            .await
    }

    pub async fn remove_session(&self, session_id: SessionId) -> Result<(), ProxydError> {
        self.expect_ok(&ToProxyd::RemoveSession(session_id), "remove session")
            .await
    }

    pub async fn sync_policies(
        &self,
        policies: Vec<engram_core::types::egress::SessionEgressPolicy>,
    ) -> Result<(), ProxydError> {
        self.expect_ok(&ToProxyd::SyncPolicies(policies), "sync policies")
            .await
    }

    pub async fn health(&self) -> Result<usize, ProxydError> {
        match self.request(&ToProxyd::Health).await? {
            FromProxyd::HealthReport { sessions } => Ok(sessions),
            other => Err(ProxydError(format!("health: unexpected reply {other:?}"))),
        }
    }

    pub async fn lookup_guest(
        &self,
        guest_ip: std::net::Ipv4Addr,
    ) -> Result<Option<engram_egress_proto::GuestSummary>, ProxydError> {
        match self.request(&ToProxyd::LookupGuest(guest_ip)).await? {
            FromProxyd::Guest(summary) => Ok(summary),
            other => Err(ProxydError(format!("lookup: unexpected reply {other:?}"))),
        }
    }

    pub async fn decide(
        &self,
        guest_ip: std::net::Ipv4Addr,
        host: &str,
    ) -> Result<Option<String>, ProxydError> {
        match self
            .request(&ToProxyd::Decide {
                guest_ip,
                host: host.to_string(),
            })
            .await?
        {
            FromProxyd::Decision(d) => Ok(d),
            other => Err(ProxydError(format!("decide: unexpected reply {other:?}"))),
        }
    }

    async fn shutdown(&self) -> Result<(), ProxydError> {
        self.expect_ok(&ToProxyd::Shutdown, "shutdown").await
    }
}

/// Ensure a serving daemon of OUR build and config, adopting a
/// survivor when possible. Errors are fatal to host-agent startup
/// (fail-closed, ADR 0083): a host whose iptables REDIRECT points at
/// a dead port must never report healthy.
pub async fn ensure_proxyd(
    cfg: &ProxydSpawnConfig,
) -> Result<(Arc<ProxydHandle>, ProxydProcess), ProxydError> {
    let handle = Arc::new(ProxydHandle::new(cfg.control_sock()));
    let ca_fingerprint = sha256_hex(cfg.ca_cert_pem.as_bytes());
    let mut expected = cfg.expected();
    expected.ca_fingerprint = &ca_fingerprint;

    // Evidence.
    let manifest = read_manifest(&cfg.work_dir);
    let identity = manifest.as_ref().and_then(|m| read_proc_identity(m.pid));
    let hello = if manifest.is_some() {
        match tokio::time::timeout(HELLO_BUDGET, handle.hello()).await {
            Ok(Ok(info)) => Some(info),
            Ok(Err(_)) | Err(_) => None,
        }
    } else {
        None
    };
    let probe_ok = match &hello {
        Some(info) => probe_accept_loop(info.proxy_port).await,
        None => false,
    };
    let evidence = LiveEvidence {
        identity: identity.clone(),
        hello,
        probe_ok,
    };
    let plan = decide_adopt(manifest.as_ref(), &evidence, &expected);
    tracing::info!(
        ?plan,
        manifest = manifest.is_some(),
        "egress proxyd adopt decision"
    );

    match plan {
        AdoptPlan::Adopt => {
            let m = manifest.expect("Adopt implies a manifest");
            match engram_sandbox_firecracker::pidfd::open_pidfd(m.pid) {
                Ok(pidfd) => {
                    tracing::info!(
                        pid = m.pid,
                        "adopted the surviving egress daemon; in-flight guest streams preserved"
                    );
                    Ok((handle, ProxydProcess::Adopted(pidfd)))
                }
                Err(e) => {
                    // Raced its exit between the probe and here.
                    tracing::warn!(pid = m.pid, error = %e, "adopt raced daemon exit; spawning fresh");
                    remove_manifest(&cfg.work_dir);
                    let child = spawn_and_confirm(cfg, &handle, &expected).await?;
                    Ok((handle, ProxydProcess::Spawned(child)))
                }
            }
        }
        AdoptPlan::RestartForUpgrade => {
            tracing::info!(
                "replacing the egress daemon (build/config changed); this node's in-flight \
                 guest streams end here — the explicit ADR 0121 tradeoff"
            );
            let _ = tokio::time::timeout(HELLO_BUDGET, handle.shutdown()).await;
            if let Some(m) = &manifest {
                await_process_gone(m.pid, EXIT_BUDGET).await;
                if read_proc_identity(m.pid).is_some() {
                    kill_pid(m.pid);
                    await_process_gone(m.pid, EXIT_BUDGET).await;
                }
            }
            remove_manifest(&cfg.work_dir);
            let child = spawn_and_confirm(cfg, &handle, &expected).await?;
            Ok((handle, ProxydProcess::Spawned(child)))
        }
        AdoptPlan::KillStaleAndSpawn => {
            let m = manifest.expect("KillStale implies a manifest");
            tracing::warn!(
                pid = m.pid,
                "killing a wedged egress daemon (identity verified)"
            );
            kill_pid(m.pid);
            await_process_gone(m.pid, EXIT_BUDGET).await;
            remove_manifest(&cfg.work_dir);
            let child = spawn_and_confirm(cfg, &handle, &expected).await?;
            Ok((handle, ProxydProcess::Spawned(child)))
        }
        AdoptPlan::SpawnFresh => {
            remove_manifest(&cfg.work_dir);
            let child = spawn_and_confirm(cfg, &handle, &expected).await?;
            Ok((handle, ProxydProcess::Spawned(child)))
        }
    }
}

/// The accept-loop probe: a TCP connect to the daemon's proxy port
/// must be accepted AND promptly closed (the registry's `NoSession`
/// drop). The close proves the dispatch path ran; a kernel backlog
/// handshake alone proves nothing. One shot at adopt/confirm time —
/// not a watchdog.
async fn probe_accept_loop(proxy_port: u16) -> bool {
    use tokio::io::AsyncReadExt as _;
    let attempt = async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", proxy_port)).await?;
        let mut buf = [0u8; 1];
        // EOF (Ok(0)) or reset — either is the prompt close. Data
        // would mean something else entirely is on this port.
        match stream.read(&mut buf).await {
            Ok(0) => Ok(()),
            Ok(_) => Err(std::io::Error::other("proxy port wrote bytes to a probe")),
            Err(_) => Ok(()),
        }
    };
    match tokio::time::timeout(PROBE_BUDGET, attempt).await {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "egress accept-loop probe failed");
            false
        }
        Err(_) => {
            tracing::warn!("egress accept-loop probe timed out (accept loop wedged?)");
            false
        }
    }
}

fn kill_pid(pid: u32) {
    // SIGKILL, not SIGTERM: this arm only runs against a daemon that
    // is provably ours (three-axis identity) and provably broken.
    if let Err(e) = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    ) {
        tracing::warn!(pid, error = %e, "kill stale egress daemon failed");
    }
}

async fn await_process_gone(pid: u32, budget: Duration) {
    let deadline = crate::time_source::metrics_now_tokio() + budget;
    while read_proc_identity(pid).is_some() {
        if crate::time_source::metrics_now_tokio() >= deadline {
            tracing::warn!(pid, "old egress daemon still alive after exit budget");
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Spawn the daemon and confirm it serves: manifest present, `Hello`
/// matches the expected config, accept-loop probe passes. The child
/// is spawned WITHOUT `kill_on_drop` (ADR 0044 K2 — it must survive
/// this process) and with stdout/stderr appended to the daemon's own
/// log file (pod logs die with the pod).
async fn spawn_and_confirm(
    cfg: &ProxydSpawnConfig,
    handle: &ProxydHandle,
    expected: &ExpectedProxyd<'_>,
) -> Result<tokio::process::Child, ProxydError> {
    let log_path = cfg.work_dir.join(engram_egress_proto::LOG_FILE_NAME);
    let open_log = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
    };
    let (out, err) = (
        open_log().map_err(|e| ProxydError(format!("open {}: {e}", log_path.display())))?,
        open_log().map_err(|e| ProxydError(format!("open {}: {e}", log_path.display())))?,
    );

    let mut cmd = tokio::process::Command::new(&cfg.bin);
    cmd.arg("--work-dir")
        .arg(&cfg.work_dir)
        .arg("--proxy-port")
        .arg(cfg.proxy_port.to_string())
        .arg("--dns-port")
        .arg(cfg.dns_port.to_string())
        .arg("--gateway-port")
        .arg(cfg.gateway_port.to_string())
        // Secrets via env: argv leaks on /proc/*/cmdline.
        .env(engram_egress_proto::ENV_CA_CERT_PEM, &cfg.ca_cert_pem)
        .env(engram_egress_proto::ENV_CA_KEY_PEM, &cfg.ca_key_pem)
        .env(engram_egress_proto::ENV_COORD_URL, &cfg.coord_url)
        .env(engram_egress_proto::ENV_HOST_ID, cfg.host_id.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err));
    if let Some(token) = &cfg.coord_token {
        cmd.env(engram_egress_proto::ENV_COORD_TOKEN, token);
    }
    if let Some(dir) = &cfg.cgroup_dir {
        cmd.arg("--cgroup-dir").arg(dir);
    }
    // NO kill_on_drop — the daemon must outlive this process (ADR
    // 0044 K2, the FC/uffd-handler discipline).
    let mut child = cmd
        .spawn()
        .map_err(|e| ProxydError(format!("spawn {}: {e}", cfg.bin.display())))?;

    let deadline = crate::time_source::metrics_now_tokio() + SPAWN_READY_BUDGET;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
            let tail = tail.lines().rev().take(12).collect::<Vec<_>>().join(" | ");
            return Err(ProxydError(format!(
                "daemon exited during startup ({status}); log tail: {tail}"
            )));
        }
        if read_manifest(&cfg.work_dir).is_some() {
            if let Ok(Ok(info)) = tokio::time::timeout(HELLO_BUDGET, handle.hello()).await {
                // Config must match what we asked for. Fingerprints are
                // NOT compared here: the spawned binary IS the deploy's
                // binary (and dev builds have none to compare).
                if info.proxy_port != expected.proxy_port
                    || info.dns_port != expected.dns_port
                    || info.gateway_port != expected.gateway_port
                    || info.ca_fingerprint != expected.ca_fingerprint
                    || info.coord_url != expected.coord_url
                {
                    return Err(ProxydError(format!(
                        "freshly spawned daemon reports foreign config: {info:?}"
                    )));
                }
                if probe_accept_loop(info.proxy_port).await {
                    tracing::info!(bin = %cfg.bin.display(), "egress daemon spawned and confirmed serving");
                    return Ok(child);
                }
            }
        }
        if crate::time_source::metrics_now_tokio() >= deadline {
            let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
            let tail = tail.lines().rev().take(12).collect::<Vec<_>>().join(" | ");
            return Err(ProxydError(format!(
                "daemon not confirmed serving within {SPAWN_READY_BUDGET:?}; log tail: {tail}"
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The replay the supervisor runs after a respawn: re-sync the
/// daemon's registry from the ADR 0111 persisted policies.
pub type PolicyReplay =
    Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

/// Event-driven supervision: await the process's exit event (child
/// `wait()` / pidfd readability), then respawn, confirm, and replay.
/// A steady-state host-agent never aborts for a daemon failure — that
/// would take the control plane down for live VMs; guests fail closed
/// (RST) until the respawn lands, which is the pre-ADR-0121 roll-gap
/// behavior, now bounded by respawn instead of pod rollout.
pub fn spawn_supervisor(
    mut process: ProxydProcess,
    handle: Arc<ProxydHandle>,
    cfg: ProxydSpawnConfig,
    replay: PolicyReplay,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            await_exit(&mut process).await;
            tracing::error!(
                "egress daemon exited; guests fail closed until respawn — respawning now"
            );
            let ca_fingerprint = sha256_hex(cfg.ca_cert_pem.as_bytes());
            let mut expected = cfg.expected();
            expected.ca_fingerprint = &ca_fingerprint;
            let mut backoff = Duration::from_millis(500);
            loop {
                remove_manifest(&cfg.work_dir);
                match spawn_and_confirm(&cfg, &handle, &expected).await {
                    Ok(child) => {
                        process = ProxydProcess::Spawned(child);
                        replay().await;
                        tracing::info!("egress daemon respawned and policies re-synced");
                        break;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "egress daemon respawn failed; retrying");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                }
            }
        }
    })
}

async fn await_exit(process: &mut ProxydProcess) {
    match process {
        ProxydProcess::Spawned(child) => {
            let _ = child.wait().await;
        }
        ProxydProcess::Adopted(pidfd) => {
            #[cfg(target_os = "linux")]
            {
                use std::os::fd::AsRawFd as _;
                // A pidfd polls readable when the process exits; wrap
                // it in AsyncFd so this await IS the exit event.
                struct RawFdWrap(std::os::fd::RawFd);
                impl std::os::fd::AsRawFd for RawFdWrap {
                    fn as_raw_fd(&self) -> std::os::fd::RawFd {
                        self.0
                    }
                }
                match tokio::io::unix::AsyncFd::with_interest(
                    RawFdWrap(pidfd.as_raw_fd()),
                    tokio::io::Interest::READABLE,
                ) {
                    Ok(afd) => {
                        let _ = afd.readable().await;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "pidfd AsyncFd failed; treating daemon as exited");
                    }
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                // Adoption requires a build fingerprint, which local
                // (non-Linux) builds never carry — unreachable in
                // practice. Treat as an immediate exit so the respawn
                // path takes over rather than hanging forever.
                let _ = pidfd;
                tracing::warn!("adopted daemon on a non-Linux host (unexpected); respawning");
            }
        }
    }
}
