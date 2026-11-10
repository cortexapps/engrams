//! The coordinator app-gRPC calls the drain makes. It runs against the
//! coordinator that still serves, so it uses only RPCs the previous release
//! has.

use std::future::Future;
use std::time::Duration;

use engram_core::HostId;
use engram_protocol::app;
use engram_protocol::app::fleet_service_client::FleetServiceClient;
use engram_protocol::app::session_service_client::SessionServiceClient;
use tonic::codegen::InterceptedService;
use tonic::transport::{Channel, Endpoint};

use crate::DrainError;

/// Sends `Authorization: Bearer <token>`. `None` omits the header, for a
/// coordinator that accepts no tokens.
#[derive(Clone)]
pub struct Bearer {
    token: Option<String>,
}

impl Bearer {
    pub fn new(token: Option<String>) -> Self {
        Self { token }
    }
}

impl tonic::service::Interceptor for Bearer {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(token) = &self.token {
            req.metadata_mut().insert(
                "authorization",
                format!("Bearer {token}")
                    .parse()
                    .map_err(|_| tonic::Status::invalid_argument("bearer token is not ASCII"))?,
            );
        }
        Ok(req)
    }
}

/// A lazily connected channel to the coordinator's app-gRPC address. An
/// address without a scheme gets `http://`.
pub fn channel(endpoint: &str, call_timeout: Duration) -> Channel {
    let endpoint = if endpoint.contains("://") {
        endpoint.to_string()
    } else {
        format!("http://{endpoint}")
    };
    Endpoint::from_shared(endpoint.clone())
        .unwrap_or_else(|e| panic!("invalid coordinator gRPC endpoint {endpoint:?}: {e}"))
        .connect_timeout(Duration::from_secs(5))
        .timeout(call_timeout)
        .connect_lazy()
}

/// How often a fleet read tries a coordinator that answers `Unavailable`,
/// and the wait between tries: about a minute in all. The hook can start
/// while the coordinator rolls, when the Service still routes to a stopping
/// pod; the Helm hook timeout bounds the whole drain.
const UNAVAILABLE_ATTEMPTS: u32 = 30;
const UNAVAILABLE_BACKOFF: Duration = Duration::from_secs(2);

/// Run one coordinator call, again on `Unavailable` (connection refused or
/// reset, no ready endpoint) up to `attempts` times. Any other status fails
/// at once.
async fn retry_unavailable<T, F, Fut>(
    op: &'static str,
    attempts: u32,
    backoff: Duration,
    mut call: F,
) -> Result<T, DrainError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<tonic::Response<T>, tonic::Status>>,
{
    let mut attempt = 1;
    loop {
        match call().await {
            Ok(resp) => return Ok(resp.into_inner()),
            Err(status) if status.code() == tonic::Code::Unavailable && attempt < attempts => {
                tracing::warn!(op, attempt, error = %status, "coordinator unavailable; retrying");
                attempt += 1;
                tokio::time::sleep(backoff).await;
            }
            Err(status) => return Err(DrainError::rpc(op, status)),
        }
    }
}

/// A read of the fleet. Its failure ends the hook, so it waits for a
/// coordinator that answers `Unavailable`.
async fn read<T, F, Fut>(op: &'static str, call: F) -> Result<T, DrainError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<tonic::Response<T>, tonic::Status>>,
{
    retry_unavailable(op, UNAVAILABLE_ATTEMPTS, UNAVAILABLE_BACKOFF, call).await
}

/// One step of a drain action: one try. The coordinator also answers
/// `Unavailable` when its own call to a host fails, and the drain sends a
/// failed action again on its next pass; a wait here would hold the other
/// actions of the pass.
async fn act<T, F, Fut>(op: &'static str, call: F) -> Result<T, DrainError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<tonic::Response<T>, tonic::Status>>,
{
    retry_unavailable(op, 1, Duration::ZERO, call).await
}

pub fn parse_host_id(id: &str) -> Result<HostId, DrainError> {
    id.parse()
        .map_err(|_| DrainError::Invalid(format!("coordinator returned malformed host id {id:?}")))
}

/// A host as the drain reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeHost {
    pub id: HostId,
    /// `ready` or `draining`: the host may still run VMs.
    pub live: bool,
    pub cordoned: bool,
    pub running_sandboxes: u32,
    pub reattach_generation: u32,
}

/// A session as the drain reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpgradeSession {
    pub id: String,
    /// The coordinator's state name (`engram_core::SessionState::as_str`).
    pub state: String,
    pub host_id: Option<HostId>,
}

pub struct Coordinator {
    fleet: FleetServiceClient<InterceptedService<Channel, Bearer>>,
    sessions: SessionServiceClient<InterceptedService<Channel, Bearer>>,
}

impl Coordinator {
    /// Fleet calls time out after 10 s. `session_timeout` bounds each
    /// session call; an eviction captures the guest, so it must cover the
    /// largest guest's capture.
    pub fn new(endpoint: &str, token: Option<String>, session_timeout: Duration) -> Self {
        Self {
            fleet: FleetServiceClient::with_interceptor(
                channel(endpoint, Duration::from_secs(10)),
                Bearer::new(token.clone()),
            ),
            sessions: SessionServiceClient::with_interceptor(
                channel(endpoint, session_timeout),
                Bearer::new(token),
            ),
        }
    }

    pub async fn list_hosts(&self) -> Result<Vec<UpgradeHost>, DrainError> {
        let resp = read("list_hosts", || {
            let mut fleet = self.fleet.clone();
            async move { fleet.list_hosts(app::ListHostsRequest {}).await }
        })
        .await?;
        resp.hosts
            .into_iter()
            .map(|v| {
                Ok(UpgradeHost {
                    id: parse_host_id(&v.id)?,
                    live: matches!(v.status.as_str(), "ready" | "draining"),
                    cordoned: v.cordoned,
                    running_sandboxes: v.running_sandboxes,
                    reattach_generation: v.reattach_generation,
                })
            })
            .collect()
    }

    pub async fn cordon(&self, host: HostId, owner: &str) -> Result<(), DrainError> {
        act("cordon", || {
            let mut fleet = self.fleet.clone();
            let req = app::CordonHostRequest {
                owner: owner.into(),
                host_id: host.to_string(),
            };
            async move { fleet.cordon_host(req).await }
        })
        .await?;
        Ok(())
    }

    pub async fn list_sessions(&self) -> Result<Vec<UpgradeSession>, DrainError> {
        let resp = read("list_sessions", || {
            let mut sessions = self.sessions.clone();
            async move { sessions.list_sessions(app::ListSessionsRequest {}).await }
        })
        .await?;
        resp.sessions
            .into_iter()
            .filter_map(|item| item.session)
            .map(|s| {
                Ok(UpgradeSession {
                    host_id: s.host_id.as_deref().map(parse_host_id).transpose()?,
                    id: s.id,
                    state: s.status,
                })
            })
            .collect()
    }

    /// Whether the session has a checkpoint it can resume from.
    pub async fn has_checkpoint(&self, id: &str) -> Result<bool, DrainError> {
        let resp = act("list_checkpoints", || {
            let mut sessions = self.sessions.clone();
            let req = app::ListCheckpointsRequest {
                session_id: id.into(),
            };
            async move { sessions.list_checkpoints(req).await }
        })
        .await?;
        Ok(resp.checkpoints.iter().any(|c| c.recoverable))
    }

    pub async fn snapshot(&self, id: &str) -> Result<(), DrainError> {
        act("snapshot", || {
            let mut sessions = self.sessions.clone();
            let req = app::SnapshotRequest {
                session_id: id.into(),
            };
            async move { sessions.snapshot(req).await }
        })
        .await?;
        Ok(())
    }

    pub async fn resume(&self, id: &str) -> Result<(), DrainError> {
        act("resume", || {
            let mut sessions = self.sessions.clone();
            let req = app::ResumeRequest {
                session_id: id.into(),
            };
            async move { sessions.resume(req).await }
        })
        .await?;
        Ok(())
    }

    /// The eviction that does not park: the session ends `idle` behind a
    /// fresh checkpoint, its VM destroyed.
    pub async fn evict(&self, id: &str) -> Result<(), DrainError> {
        act("evict_local", || {
            let mut sessions = self.sessions.clone();
            let req = app::EvictLocalRequest {
                session_id: id.into(),
            };
            async move { sessions.evict_local(req).await }
        })
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// A call that answers each status of `script` in turn, then `Ok(7)`.
    fn scripted(
        script: Vec<tonic::Code>,
        calls: &AtomicU32,
    ) -> impl FnMut() -> std::future::Ready<Result<tonic::Response<u32>, tonic::Status>> + '_ {
        move || {
            let n = calls.fetch_add(1, Ordering::SeqCst) as usize;
            std::future::ready(match script.get(n) {
                Some(code) => Err(tonic::Status::new(*code, "scripted")),
                None => Ok(tonic::Response::new(7)),
            })
        }
    }

    #[tokio::test]
    async fn an_unavailable_coordinator_is_tried_again_until_it_answers() {
        let calls = AtomicU32::new(0);
        let script = vec![tonic::Code::Unavailable, tonic::Code::Unavailable];
        let got = retry_unavailable("list_hosts", 5, Duration::ZERO, scripted(script, &calls))
            .await
            .unwrap();
        assert_eq!((got, calls.load(Ordering::SeqCst)), (7, 3));
    }

    #[tokio::test]
    async fn any_other_status_fails_at_once() {
        let calls = AtomicU32::new(0);
        let script = vec![tonic::Code::PermissionDenied];
        let err = retry_unavailable("list_hosts", 5, Duration::ZERO, scripted(script, &calls))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            DrainError::Rpc {
                op: "list_hosts",
                ..
            }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_retries_stop_after_the_last_attempt() {
        let calls = AtomicU32::new(0);
        let script = vec![tonic::Code::Unavailable; 10];
        let err = retry_unavailable("list_hosts", 3, Duration::ZERO, scripted(script, &calls))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, DrainError::Rpc { status, .. } if status.code() == tonic::Code::Unavailable)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn an_action_gets_one_try() {
        let calls = AtomicU32::new(0);
        let script = vec![tonic::Code::Unavailable];
        let err = act("evict_local", scripted(script, &calls))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, DrainError::Rpc { status, .. } if status.code() == tonic::Code::Unavailable)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
