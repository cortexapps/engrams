//! Host-agent-owned egress proxy.
//!
//! ADR 0006: each FC host-agent runs its own TLS-MITM proxy.
//! iptables REDIRECT on the FC host hands tcp/443 from every guest
//! to this proxy. The proxy looks up the source IP in the local
//! registry (populated by `SandboxBackend::notify_session_policy`
//! frames from the coordinator), applies the manifest's network
//! allow-list at SNI, and substitutes per-secret placeholders for
//! broker-mode images after MITM.
//!
//! Every host-agent in a deployment loads the same CA material via
//! a [`CaSource`] impl so guest substrates that trust one host's
//! leaves trust them all.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use engram_core::types::integration::{CredentialPurpose, SessionTunnel};
use engram_core::{HostId, SessionId};
use engram_egress_proxy::{
    CaSource, CertMint, GuestGatewayRegistry, InjectRefresher, Listeners, Proxy, ProxyConfig,
    RefreshedInject, Registry, TunnelConnector,
};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};

use crate::coord_client::HttpCoordClient;

/// WS4: the host-agent's [`InjectRefresher`] — bridges the egress proxy's
/// near-expiry re-mint request to the coordinator's inject-refresh route (which
/// holds the mint authority). The proxy AWAITS this before injecting a stale
/// minted credential, closing the campaign's reads-401/writes-succeed asymmetry.
pub struct CoordInjectRefresher {
    coord: HttpCoordClient,
    host_id: HostId,
}

impl CoordInjectRefresher {
    pub fn new(coord: HttpCoordClient, host_id: HostId) -> Self {
        Self { coord, host_id }
    }
}

#[async_trait]
impl InjectRefresher for CoordInjectRefresher {
    async fn refresh(
        &self,
        session_id: SessionId,
        mint_source: &engram_core::types::integration::CredentialMintSource,
    ) -> Option<RefreshedInject> {
        match self
            .coord
            .refresh_inject(self.host_id, session_id, mint_source)
            .await
        {
            Ok(resp) => Some(RefreshedInject {
                secret: resp.secret,
                expires_at: resp.expires_at,
            }),
            Err(e) => {
                // The proxy keeps the stale secret on `None` — a stale token 401s
                // (recoverable), and a coord blip must not drop the guest's request.
                tracing::warn!(%session_id, source = ?mint_source, error = %e, "egress inject re-mint via coord failed");
                None
            }
        }
    }
}

/// Starts one host-local Cloud SQL Auth Proxy per guest database connection.
/// OAuth tokens are passed only through the child environment.
pub struct CoordCloudSqlConnector {
    coord: HttpCoordClient,
    host_id: HostId,
}

const CLOUD_SQL_CONNECTOR_KIND: &str = "gcp.cloud_sql";
const CLOUD_SQL_STDERR_TAIL_BYTES: usize = 8 * 1024;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CloudSqlTunnelConfig {
    instance: String,
    database_user: String,
}

impl CoordCloudSqlConnector {
    pub fn new(coord: HttpCoordClient, host_id: HostId) -> Self {
        Self { coord, host_id }
    }

    async fn token(
        &self,
        session_id: SessionId,
        tunnel: &SessionTunnel,
        purpose: CredentialPurpose,
    ) -> std::io::Result<String> {
        let mint_source = tunnel.mint_source.as_ref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Cloud SQL tunnel has no credential mint source",
            )
        })?;
        self.coord
            .mint_connection_credential(self.host_id, session_id, mint_source, purpose)
            .await
            .map(|response| response.secret)
            .map_err(|error| std::io::Error::other(error.to_string()))
    }

    async fn relay_inner(
        &self,
        downstream: &mut TcpStream,
        initial_data: Vec<u8>,
        session_id: SessionId,
        tunnel: SessionTunnel,
        established: &mut bool,
    ) -> std::io::Result<()> {
        let config: CloudSqlTunnelConfig = serde_json::from_str(&tunnel.config_json)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let mint_start = crate::time_source::metrics_now();
        let (api_token, login_token) = tokio::try_join!(
            self.token(
                session_id,
                &tunnel,
                CredentialPurpose::new("cloud_sql_admin")
            ),
            self.token(
                session_id,
                &tunnel,
                CredentialPurpose::new("cloud_sql_login")
            ),
        )?;
        let mint_ms = mint_start.elapsed().as_millis() as u64;
        // A private directory gives each relay its own socket namespace. A
        // bind-then-drop TCP reservation can be stolen by another session
        // before the child binds, which can cross-connect two tenants.
        let socket_dir = cloud_sql_socket_dir()?;
        let listen_dir = cloud_sql_listen_dir(socket_dir.path());
        let socket_path = cloud_sql_postgres_socket_path(socket_dir.path());

        let binary = std::env::var_os("ENGRAM_CLOUD_SQL_PROXY")
            .unwrap_or_else(|| "/usr/local/bin/cloud-sql-proxy".into());
        let spawn_start = crate::time_source::metrics_now();
        let mut child = tokio::process::Command::new(binary)
            // `unix-socket-path` pins the listen directory. `--unix-socket`
            // would instead append the instance connection name, so a long
            // name pushes the socket past the 108-byte `sockaddr_un` limit and
            // the proxy exits with "bind: invalid argument".
            .arg(format!(
                "{}?unix-socket-path={}",
                config.instance,
                listen_dir.display()
            ))
            .arg("--auto-iam-authn")
            .arg("--max-connections=1")
            // `--lazy-refresh` disables cloudsqlconn's refresh-ahead cache.
            // The refresh-ahead path judges every ephemeral cert against the
            // login token's oauth2 `Expiry`, which a static `--login-token`
            // leaves at the zero time — every cert reads as already expired,
            // the cache churns refreshes, and its 30-second rate limiter
            // blocks ~25% of dials for exactly 30s (issue #1201). The lazy
            // cache has no limiter and refreshes inline on each dial.
            .arg("--lazy-refresh")
            // Do not inherit host-agent credentials or deployment secrets.
            .env_clear()
            .env("CSQL_PROXY_TOKEN", api_token)
            .env("CSQL_PROXY_LOGIN_TOKEN", login_token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stderr = child
            .stderr
            .take()
            .expect("stderr was configured as a pipe");
        let stderr_task = tokio::spawn(collect_stderr_tail(stderr));

        let mut upstream = None;
        let mut last_connect_error = None;
        for _ in 0..100 {
            if let Some(status) = child.try_wait()? {
                let stderr = stderr_task.await.unwrap_or_default();
                return Err(std::io::Error::other(format!(
                    "Cloud SQL Auth Proxy exited before accepting a connection: {status}{}",
                    stderr_context(&stderr),
                )));
            }
            match UnixStream::connect(&socket_path).await {
                Ok(stream) => {
                    upstream = Some(stream);
                    break;
                }
                Err(error) => {
                    last_connect_error = Some(error);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
        let Some(mut upstream) = upstream else {
            let _ = child.kill().await;
            let stderr = stderr_task.await.unwrap_or_default();
            let connect_context = last_connect_error
                .map(|error| format!("; last socket error: {error}"))
                .unwrap_or_default();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "Cloud SQL Auth Proxy did not become ready{connect_context}{}",
                    stderr_context(&stderr),
                ),
            ));
        };

        let socket_wait_ms = spawn_start.elapsed().as_millis() as u64;

        downstream
            .write_all(b"HTTP/1.1 200 Connection Established\r\nEngram-Gateway: 1\r\n\r\n")
            .await?;
        *established = true;
        if !initial_data.is_empty() {
            upstream.write_all(&initial_data).await?;
        }
        // With `--lazy-refresh` the child binds its socket before any API
        // call, so `socket_wait_ms` is pure spawn+bind; the first byte the
        // guest relays then pays the ephemeral-cert fetch inline.
        tracing::info!(
            %session_id,
            tunnel_id = %tunnel.id,
            instance = %config.instance,
            database_user = %config.database_user,
            mint_ms,
            socket_wait_ms,
            "authorized session tunnel",
        );
        let relay_result = tokio::io::copy_bidirectional(downstream, &mut upstream).await;
        let _ = child.kill().await;
        let _ = stderr_task.await;
        relay_result.map(|_| ())
    }
}

#[async_trait]
impl TunnelConnector for CoordCloudSqlConnector {
    fn kind(&self) -> &'static str {
        CLOUD_SQL_CONNECTOR_KIND
    }

    async fn relay(
        &self,
        mut downstream: TcpStream,
        initial_data: Vec<u8>,
        session_id: SessionId,
        tunnel: SessionTunnel,
    ) -> std::io::Result<()> {
        let mut established = false;
        let result = self
            .relay_inner(
                &mut downstream,
                initial_data,
                session_id,
                tunnel,
                &mut established,
            )
            .await;
        if let Err(error) = &result {
            if !established {
                let _ = write_tunnel_failure(&mut downstream, error).await;
            }
        }
        result
    }
}

fn cloud_sql_socket_dir() -> std::io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("engram-cloud-sql-")
        .tempdir()
}

/// The directory the proxy listens in. The proxy creates this one level below
/// the relay's private directory, so the name is a fixed component and the
/// socket path stays the same length for every instance.
fn cloud_sql_listen_dir(socket_dir: &Path) -> PathBuf {
    socket_dir.join("db")
}

fn cloud_sql_postgres_socket_path(socket_dir: &Path) -> PathBuf {
    cloud_sql_listen_dir(socket_dir).join(".s.PGSQL.5432")
}

async fn collect_stderr_tail(mut stderr: tokio::process::ChildStderr) -> Vec<u8> {
    let mut tail = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        match stderr.read(&mut chunk).await {
            Ok(0) | Err(_) => return tail,
            Ok(read) => append_tail(&mut tail, &chunk[..read]),
        }
    }
}

fn append_tail(tail: &mut Vec<u8>, chunk: &[u8]) {
    tail.extend_from_slice(chunk);
    if tail.len() > CLOUD_SQL_STDERR_TAIL_BYTES {
        tail.drain(..tail.len() - CLOUD_SQL_STDERR_TAIL_BYTES);
    }
}

fn stderr_context(stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        String::new()
    } else {
        format!("; stderr: {stderr}")
    }
}

async fn write_tunnel_failure(
    downstream: &mut (impl AsyncWrite + Unpin),
    error: &std::io::Error,
) -> std::io::Result<()> {
    let body = format!("Cloud SQL tunnel failed: {error}\n");
    let response = format!(
        "HTTP/1.1 502 Bad Gateway\r\nEngram-Gateway: 1\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    downstream.write_all(response.as_bytes()).await
}

/// Per-host egress-proxy handle. Holds the registry (mutated as
/// sessions come and go on this host), the CA cert PEM (handed to
/// `ensure_harness_ext4` so every guest substrate this host builds
/// trusts our leaves), and the spawned listener task.
pub struct HostEgress {
    pub registry: Arc<Registry>,
    /// CA cert PEM. Stamped into every harness substrate this host
    /// builds so the guest's trust store accepts our MITM leaves.
    pub ca_cert_pem: String,
    /// Listener task. Held for its lifetime; dropping the
    /// `HostEgress` doesn't abort the task because the registry +
    /// mint are held inside the proxy via Arc, so the future
    /// stays valid. We keep the handle anyway so callers can
    /// observe spawn/error.
    _proxy_task: tokio::task::JoinHandle<()>,
}

/// Reasons proxy spawn-up can fail. Distinguishes CA loading
/// (typically a deployment misconfig) from binding (port in use).
#[derive(Debug)]
pub enum EgressError {
    Ca(engram_egress_proxy::CaError),
    /// CA loaded but the listener couldn't bind. Returned with the
    /// underlying io error so operators see the exact reason.
    Bind(std::io::Error),
}

impl std::fmt::Display for EgressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ca(e) => write!(f, "egress CA: {e}"),
            Self::Bind(e) => write!(f, "egress proxy bind: {e}"),
        }
    }
}

impl std::error::Error for EgressError {}

impl HostEgress {
    /// Load the CA via the supplied source, build the proxy, **bind its
    /// listeners synchronously**, then spawn the accept loop.
    ///
    /// Fail-closed on a bind failure. Egress is mandatory (issue #240):
    /// the caller (`main.rs`) aborts host-agent startup on
    /// `EgressError::Bind`, because a host that keeps running with a
    /// live iptables `:443 -> proxy` REDIRECT but no listener sends
    /// every guest a RST — the guest's TLS client reports
    /// `ConnectionRefused` — silently breaking every session on the
    /// host. Binding here (rather than inside the accept-loop task)
    /// is what turns that failure into a value the caller can act on;
    /// the earlier design spawned the bind inside a detached task, so
    /// `spawn` returned `Ok` before the bind was even attempted and a
    /// failure only surfaced as a log line from the dying task.
    /// `dns_bind_addr`: where the filtering DNS proxy binds (both UDP
    /// and TCP). Production passes the port the iptables `:53 -> dns`
    /// REDIRECT targets; it must match or the guest can't resolve.
    /// `None` disables the DNS listener entirely — used by tests that
    /// exercise only the egress registry and would otherwise collide on
    /// the fixed DNS port when the suite runs in parallel.
    pub async fn spawn(
        ca_source: Arc<dyn CaSource>,
        bind_addr: SocketAddr,
        dns_bind_addr: Option<SocketAddr>,
        guest_gateway_bind_addr: Option<SocketAddr>,
        observe_sink: Option<engram_egress_proxy::ObserveSink>,
        inject_refresher: Option<Arc<dyn InjectRefresher>>,
        guest_gateway: Arc<GuestGatewayRegistry>,
    ) -> Result<Self, EgressError> {
        let ca = ca_source.load().await.map_err(EgressError::Ca)?;
        let ca_cert_pem = ca.cert_pem.clone();

        // Install rustls's default crypto provider once. The proxy
        // signs leaves via a single global provider; `install_default`
        // returns Err if already set (typical when --mode=all runs
        // both coord and host-agent in-process and another caller
        // set the provider first). Ignore that.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let registry = Arc::new(Registry::new());
        let mint = Arc::new(CertMint::new(Arc::new(ca)));

        let mut proxy_cfg = ProxyConfig::new(bind_addr, registry.clone(), mint);
        proxy_cfg.dns_bind_addr = dns_bind_addr;
        proxy_cfg.guest_gateway_bind_addr = guest_gateway_bind_addr;
        proxy_cfg.observe_sink = observe_sink;
        proxy_cfg.inject_refresher = inject_refresher;
        proxy_cfg.guest_gateway = guest_gateway;
        let proxy = Proxy::new(proxy_cfg);

        let listeners = bind_with_retry(&proxy).await.map_err(EgressError::Bind)?;
        let task = tokio::spawn(async move {
            proxy.serve(listeners).await;
            // `serve` loops forever on accept; if it ever returns, the
            // proxy is down while iptables still REDIRECTs to it —
            // log loudly so operators aren't left diagnosing silent
            // per-session `ConnectionRefused`.
            tracing::error!("egress proxy serve loop exited unexpectedly");
        });
        tracing::info!(addr = %bind_addr, "host-agent egress proxy spawned");

        Ok(Self {
            registry,
            ca_cert_pem,
            _proxy_task: task,
        })
    }
}

/// Bind the proxy listeners, retrying briefly to ride over a transient
/// port race — e.g. a host-agent restart racing the previous instance's
/// socket teardown, which is exactly how the proxy came up dead on the
/// fc-colima dev rig (both ports were free moments later). A bind that
/// still fails after the retry budget is fatal: `spawn` returns
/// `EgressError::Bind` and `main.rs` aborts (fail-closed). Under a
/// supervisor (K8s, or a Tilt retrigger) a permanent conflict then
/// crashloops loudly instead of serving broken sessions.
async fn bind_with_retry(proxy: &Proxy) -> Result<Listeners, std::io::Error> {
    const ATTEMPTS: u32 = 5;
    const BACKOFF: Duration = Duration::from_millis(500);
    let mut last_err = None;
    for attempt in 1..=ATTEMPTS {
        match proxy.bind().await {
            Ok(bound) => return Ok(bound),
            Err(e) => {
                tracing::warn!(
                    attempt,
                    max_attempts = ATTEMPTS,
                    error = %e,
                    "egress proxy bind failed; retrying",
                );
                last_err = Some(e);
                if attempt < ATTEMPTS {
                    tokio::time::sleep(BACKOFF).await;
                }
            }
        }
    }
    Err(last_err.expect("loop runs at least once, so last_err is set on failure"))
}

/// ADR 0059: build the proxy's optional GraphQL matcher from the wire fields.
/// `Ok(None)` = a REST entry (empty op); `Ok(Some(_))` = a valid GraphQL entry;
/// `Err(())` = a non-empty but unparseable op (a corrupt entry — the caller skips
/// it, fail-closed, so it never degrades into a permissive REST gate on `/graphql`).
fn graphql_match(op: &str, field: &str) -> Result<Option<engram_egress_proxy::GraphqlMatch>, ()> {
    if op.is_empty() {
        return Ok(None);
    }
    match engram_egress_proxy::GraphqlOperation::parse(op) {
        Some(operation) => Ok(Some(engram_egress_proxy::GraphqlMatch {
            operation,
            field: field.to_string(),
        })),
        None => Err(()),
    }
}

/// Translate an incoming wire policy into the proxy's `SessionState`
/// shape and register it against `guest_ip`. The proxy looks up
/// sessions by IP on every connection — this is the source of
/// truth for "does the proxy know about this session yet?".
///
/// Idempotent on re-registration: `Registry::register` replaces
/// any prior entry for the same IP, so a re-issued policy frame
/// (e.g. on cold resume to a new host that previously held the
/// session) updates cleanly.
pub fn register_policy(
    registry: &Registry,
    policy: engram_core::types::egress::SessionEgressPolicy,
) -> Result<(), engram_egress_proxy::policy::ParseError> {
    let network_allow = engram_egress_proxy::HostList::from_manifest(
        &policy.network_allow_hosts,
        &policy.network_allow_host_patterns,
    )?;
    let allow_all = policy.allow_all;
    let mut secrets = Vec::with_capacity(policy.secrets.len());
    for s in policy.secrets {
        let allow =
            engram_egress_proxy::HostList::from_manifest(&s.allow_hosts, &s.allow_host_patterns)?;
        secrets.push(engram_egress_proxy::SecretEntry {
            placeholder: s.placeholder,
            real_value: s.real_value,
            allow,
        });
    }
    // ADR 0056 Plane B: the coordinator already resolved each inject's
    // secret_ref → real `secret` (host-side); translate into the proxy's
    // InjectEntry + RequestPolicy.
    let mut injects = Vec::with_capacity(policy.injects.len());
    for i in policy.injects {
        let allow =
            engram_egress_proxy::HostList::from_manifest(&i.allow_hosts, &i.allow_host_patterns)?;
        // ADR 0059: a non-empty-but-unparseable graphql op is a corrupt entry —
        // skip it (fail-closed) so it never degrades into a permissive REST gate
        // on `/graphql`.
        let graphql = match graphql_match(&i.graphql_operation, &i.graphql_field) {
            Ok(g) => g,
            Err(()) => {
                tracing::warn!(
                    op = %i.graphql_operation,
                    "skipping inject with unparseable graphql operation",
                );
                continue;
            }
        };
        injects.push(engram_egress_proxy::InjectEntry {
            header_name: i.header_name,
            header_template: i.header_template,
            allow,
            policy: engram_egress_proxy::RequestPolicy {
                methods: i.methods,
                path_globs: i.path_globs,
                graphql,
            },
            // WS4: a minted entry (non-empty provider) carries a TTL the proxy
            // re-mints against near expiry; a static secret has neither.
            mint_source: i.mint_source,
            cred: engram_egress_proxy::RefreshableCred::new(i.secret, i.expires_at),
        });
    }
    // ADR 0056 Phase 4: translate the policy's observe specs (no secret to
    // resolve — the asset map is pure) into the proxy's ObserveEntry. The proxy
    // emits an IntegrationAsset from a matching request's real response.
    let mut observes = Vec::with_capacity(policy.observes.len());
    for o in policy.observes {
        let allow =
            engram_egress_proxy::HostList::from_manifest(&o.allow_hosts, &o.allow_host_patterns)?;
        let graphql = match graphql_match(&o.graphql_operation, &o.graphql_field) {
            Ok(g) => g,
            Err(()) => {
                tracing::warn!(
                    op = %o.graphql_operation,
                    "skipping observe with unparseable graphql operation",
                );
                continue;
            }
        };
        // ADR 0059: a GraphQL observe gates success on the absence of top-level
        // `errors`; a REST observe on the 2xx status class (else Always).
        let success = if o.success_no_graphql_errors {
            engram_egress_proxy::SuccessRule::NoGraphqlErrors
        } else {
            match o.success_status_class.as_deref() {
                Some("2xx") => engram_egress_proxy::SuccessRule::StatusClass2xx,
                _ => engram_egress_proxy::SuccessRule::Always,
            }
        };
        observes.push(engram_egress_proxy::ObserveEntry {
            allow,
            policy: engram_egress_proxy::RequestPolicy {
                methods: o.methods,
                path_globs: o.path_globs,
                graphql,
            },
            provider: o.provider,
            asset_kind: o.asset_kind,
            surface: o.surface,
            success,
            data: o.data,
            fetchable: o.fetchable,
            url_fallback: o.url_fallback.map(|f| engram_egress_proxy::UrlFallback {
                pattern: f.pattern,
                fields: f.fields,
            }),
        });
    }
    registry.register(engram_egress_proxy::SessionState {
        session_id: policy.session_id,
        guest_ip: policy.guest_ip,
        network_allow,
        allow_all,
        secrets,
        injects,
        observes,
        guest_services: policy.guest_services,
        tunnels: policy.tunnels,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    // tests drive a live system; wall clock/OS entropy here is input, not a decision source (ADR 0098 D1)
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use engram_core::types::egress::{EgressInjectEntry, EgressObserveEntry, SessionEgressPolicy};
    use engram_core::types::image::SecretMode;
    use engram_core::types::integration::{CredentialMintSource, SessionTunnel};
    use engram_core::{SandboxId, SessionId};
    use std::net::Ipv4Addr;

    #[test]
    fn cloud_sql_relays_get_distinct_socket_namespaces() {
        let first = cloud_sql_socket_dir().unwrap();
        let second = cloud_sql_socket_dir().unwrap();
        assert_ne!(first.path(), second.path());
    }

    #[tokio::test]
    async fn cloud_sql_relay_connects_to_the_postgres_proxy_socket() {
        // macOS limits Unix socket paths to 103 bytes. Use the system's short
        // temp alias so this proxy-layout regression also runs in macOS CI.
        let socket_dir = tempfile::Builder::new()
            .prefix("csql-")
            .tempdir_in("/tmp")
            .unwrap();
        let listen_dir = cloud_sql_listen_dir(socket_dir.path());
        std::fs::create_dir(&listen_dir).unwrap();
        let proxy_socket = listen_dir.join(".s.PGSQL.5432");
        let _listener = tokio::net::UnixListener::bind(&proxy_socket).unwrap();

        UnixStream::connect(cloud_sql_postgres_socket_path(socket_dir.path()))
            .await
            .unwrap();
    }

    #[test]
    fn cloud_sql_socket_path_stays_inside_the_sockaddr_un_limit() {
        // `sockaddr_un.sun_path` holds 108 bytes on Linux and 104 on macOS.
        // The listen directory carries a fixed name, so no instance connection
        // name can push the socket over either limit. A 66-character name
        // reached 109 bytes when the proxy appended it instead.
        let socket_dir = cloud_sql_socket_dir().unwrap();
        let socket_path = cloud_sql_postgres_socket_path(socket_dir.path());
        let length = socket_path.as_os_str().len();
        assert!(
            length < 104,
            "socket path is {length} bytes: {}",
            socket_path.display()
        );
    }

    #[test]
    fn cloud_sql_stderr_tail_is_bounded_and_keeps_the_newest_bytes() {
        let mut tail = Vec::new();
        append_tail(&mut tail, &vec![b'a'; CLOUD_SQL_STDERR_TAIL_BYTES]);
        append_tail(&mut tail, b"diagnostic");
        assert_eq!(tail.len(), CLOUD_SQL_STDERR_TAIL_BYTES);
        assert!(tail.ends_with(b"diagnostic"));
    }

    #[tokio::test]
    async fn cloud_sql_setup_failure_returns_an_actionable_bad_gateway() {
        let (mut downstream, mut client) = tokio::io::duplex(4096);
        let error = std::io::Error::other("invalid instance");
        write_tunnel_failure(&mut downstream, &error).await.unwrap();
        drop(downstream);
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 502 Bad Gateway\r\n"));
        assert!(response.contains("Cloud SQL tunnel failed: invalid instance\n"));
    }

    /// ADR 0056 (B′): `register_policy` translates a wire `EgressInjectEntry`
    /// (secret already resolved by the coordinator) into the proxy's
    /// `InjectEntry` + `RequestPolicy` the 3a engine enforces.
    #[test]
    fn register_policy_translates_injects_to_proxy_entries() {
        let registry = Registry::new();
        let guest_ip = Ipv4Addr::new(10, 200, 0, 2);
        register_policy(
            &registry,
            SessionEgressPolicy {
                session_id: SessionId::new(),
                sandbox_id: SandboxId::new(),
                guest_ip,
                network_allow_hosts: vec![],
                network_allow_host_patterns: vec![],
                allow_all: false,
                secrets: vec![],
                injects: vec![
                    EgressInjectEntry {
                        secret: "dd-secret".into(),
                        header_name: "DD-API-KEY".into(),
                        header_template: "{}".into(),
                        allow_hosts: vec!["api.datadoghq.com".into()],
                        allow_host_patterns: vec![],
                        methods: vec!["GET".into()],
                        path_globs: vec!["/api/v2/logs*".into()],
                        graphql_operation: String::new(),
                        graphql_field: String::new(),
                        mint_source: None,
                        expires_at: None,
                    },
                    // ADR 0059: a GraphQL inject (gated by operation+field).
                    // WS4: a minted (refreshable) github entry with a TTL.
                    EgressInjectEntry {
                        secret: "gh-token".into(),
                        header_name: "Authorization".into(),
                        header_template: "Bearer {}".into(),
                        allow_hosts: vec!["api.github.com".into()],
                        allow_host_patterns: vec![],
                        methods: vec!["POST".into()],
                        path_globs: vec!["/graphql".into()],
                        graphql_operation: "mutation".into(),
                        graphql_field: "mergePullRequest".into(),
                        mint_source: Some(
                            engram_core::types::integration::CredentialMintSource::Connection {
                                connection_id: "github-default".into(),
                                provider: "github".into(),
                            },
                        ),
                        expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                    },
                ],
                observes: vec![
                    EgressObserveEntry {
                        allow_hosts: vec!["api.github.com".into()],
                        allow_host_patterns: vec![],
                        methods: vec!["POST".into()],
                        path_globs: vec!["/repos/*/issues".into()],
                        provider: "github".into(),
                        asset_kind: "issue".into(),
                        surface: "asset".into(),
                        success_status_class: Some("2xx".into()),
                        success_no_graphql_errors: false,
                        graphql_operation: String::new(),
                        graphql_field: String::new(),
                        data: vec![("number".into(), "$.resp.number".into())],
                        fetchable: Some("$.resp.html_url".into()),
                        url_fallback: None,
                    },
                    // ADR 0059: a GraphQL observe (NoGraphqlErrors success rule).
                    EgressObserveEntry {
                        allow_hosts: vec!["api.github.com".into()],
                        allow_host_patterns: vec![],
                        methods: vec!["POST".into()],
                        path_globs: vec!["/graphql".into()],
                        provider: "github".into(),
                        asset_kind: "issue".into(),
                        surface: "asset".into(),
                        success_status_class: None,
                        success_no_graphql_errors: true,
                        graphql_operation: "mutation".into(),
                        graphql_field: "createIssue".into(),
                        data: vec![("id".into(), "$.resp.data.createIssue.issue.id".into())],
                        fetchable: None,
                        url_fallback: Some(engram_core::types::integration::ObserveUrlFallback {
                            pattern: "https://github.com/{owner}/{name}/issues/{number:int}".into(),
                            fields: vec![("number".into(), "{number}".into())],
                        }),
                    },
                ],
                guest_services: Vec::new(),
                tunnels: vec![SessionTunnel {
                    id: "prod-readonly".into(),
                    connector: "gcp.cloud_sql".into(),
                    config_json: r#"{"instance":"customer:us-central1:prod","database_user":"reader@customer.iam"}"#.into(),
                    mint_source: Some(CredentialMintSource::Connection {
                        connection_id: "gcp-prod".into(),
                        provider: "gcp".into(),
                    }),
                }],
                secret_mode: SecretMode::Broker,
            },
        )
        .expect("register");

        let state = registry.lookup(guest_ip).expect("session registered");
        assert_eq!(state.tunnels[0].id, "prod-readonly");
        assert_eq!(state.injects.len(), 2);
        let inj = &state.injects[0];
        assert_eq!(inj.secret(), "dd-secret");
        assert!(inj.mint_source.is_none()); // static: never refreshed
        assert_eq!(inj.header_name, "DD-API-KEY");
        assert!(inj.allow.matches("api.datadoghq.com"));
        assert!(inj.policy.allows("GET", "/api/v2/logs/events"));
        assert!(!inj.policy.allows("POST", "/api/v2/logs/events"));
        assert!(
            inj.policy.graphql.is_none(),
            "REST inject has no graphql matcher"
        );

        // ADR 0059: the GraphQL inject translates into a RequestPolicy.graphql.
        let gql_inj = &state.injects[1];
        assert_eq!(gql_inj.secret(), "gh-token");
        assert_eq!(
            gql_inj.mint_source,
            Some(
                engram_core::types::integration::CredentialMintSource::Connection {
                    connection_id: "github-default".into(),
                    provider: "github".into(),
                }
            )
        ); // WS4: refreshable
        let g = gql_inj
            .policy
            .graphql
            .as_ref()
            .expect("graphql matcher present");
        assert_eq!(g.operation, engram_egress_proxy::GraphqlOperation::Mutation);
        assert_eq!(g.field, "mergePullRequest");
        assert!(gql_inj.policy.path_matches("/graphql"));

        // ADR 0056 Phase 4: the REST observe spec translates into a proxy ObserveEntry.
        assert_eq!(state.observes.len(), 2);
        let obs = &state.observes[0];
        assert_eq!(obs.provider, "github");
        assert_eq!(obs.asset_kind, "issue");
        assert_eq!(obs.surface, "asset");
        assert!(obs.allow.matches("api.github.com"));
        assert!(obs.policy.allows("POST", "/repos/x/issues"));
        assert!(matches!(
            obs.success,
            engram_egress_proxy::SuccessRule::StatusClass2xx
        ));
        assert_eq!(obs.fetchable.as_deref(), Some("$.resp.html_url"));

        // ADR 0059: the GraphQL observe maps to a graphql matcher + NoGraphqlErrors.
        let gql_obs = &state.observes[1];
        let go = gql_obs
            .policy
            .graphql
            .as_ref()
            .expect("graphql matcher present");
        assert_eq!(
            go.operation,
            engram_egress_proxy::GraphqlOperation::Mutation
        );
        assert_eq!(go.field, "createIssue");
        assert!(matches!(
            gql_obs.success,
            engram_egress_proxy::SuccessRule::NoGraphqlErrors
        ));
        // GraphQL parity: the URL fallback rides through to the proxy entry.
        let fb = gql_obs.url_fallback.as_ref().expect("url fallback present");
        assert_eq!(
            fb.pattern,
            "https://github.com/{owner}/{name}/issues/{number:int}"
        );
        assert_eq!(fb.fields, vec![("number".into(), "{number}".into())]);
        assert_eq!(state.observes[0].url_fallback, None);
    }
}
