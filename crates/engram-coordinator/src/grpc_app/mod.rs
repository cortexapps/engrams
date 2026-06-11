//! Orchestrator-facing app gRPC surface (ADR 0039 §2.3). Lives beside
//! the axum API during the migration; the axum web routes retire in
//! Phase 5.
//!
//! Task 8 scaffold: every RPC answers `Code::Unimplemented`. Tasks
//! 10-13 replace the stubs service-by-service with extract-and-delegate
//! implementations over the same `AppState` the axum handlers use.
//! The caller is a single trusted service (the orchestrator,
//! bearer-authed from Task 9); per-user authz lives over there, not
//! here.

use std::pin::Pin;
use std::time::Duration;

use engram_protocol::app;
use tonic::{Request, Response, Status};

use crate::state::SharedState;

/// Boxed response stream for the server-streaming RPCs. The concrete
/// streams arrive with the real implementations (Tasks 11-12); the
/// stubs only need the associated types to satisfy the traits.
type BoxStream<T> = Pin<Box<dyn tokio_stream::Stream<Item = Result<T, Status>> + Send>>;

const UNIMPLEMENTED: &str = "ADR 0039 phase 2";

/// Build the tonic server for the app surface: all five services on
/// one listener, HTTP/2 keepalives per ADR 0039 §9.4 so a dead
/// orchestrator's streams are detected and torn down (releasing
/// leases/subscriptions) instead of leaking until TCP gives up.
///
/// Returns the un-bound router; the caller picks the bind strategy
/// (`serve_with_shutdown` in lib.rs, `serve_with_incoming` on an
/// ephemeral port in tests).
pub fn server(state: SharedState) -> tonic::transport::server::Router {
    tonic::transport::Server::builder()
        // ADR §9.4: keepalive PINGs so a dead orchestrator's streams
        // are detected and torn down (releases leases/subscriptions).
        .http2_keepalive_interval(Some(Duration::from_secs(20)))
        .http2_keepalive_timeout(Some(Duration::from_secs(10)))
        .add_service(app::session_service_server::SessionServiceServer::new(
            AppSessionService {
                state: state.clone(),
            },
        ))
        .add_service(
            app::shell_relay_service_server::ShellRelayServiceServer::new(AppShellRelayService {
                state: state.clone(),
            }),
        )
        .add_service(app::fleet_service_server::FleetServiceServer::new(
            AppFleetService {
                state: state.clone(),
            },
        ))
        .add_service(app::image_service_server::ImageServiceServer::new(
            AppImageService {
                state: state.clone(),
            },
        ))
        .add_service(app::secret_service_server::SecretServiceServer::new(
            AppSecretService { state },
        ))
}

pub struct AppSessionService {
    // Consumed from Task 10 onward; the scaffold only carries it.
    #[allow(dead_code)]
    pub state: SharedState,
    // auth field arrives in Task 9
}

#[tonic::async_trait]
impl app::session_service_server::SessionService for AppSessionService {
    async fn list_sessions(
        &self,
        _req: Request<app::ListSessionsRequest>,
    ) -> Result<Response<app::ListSessionsResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn create_session(
        &self,
        _req: Request<app::CreateSessionRequest>,
    ) -> Result<Response<app::CreateSessionResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn get_session(
        &self,
        _req: Request<app::GetSessionRequest>,
    ) -> Result<Response<app::GetSessionResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn delete_session(
        &self,
        _req: Request<app::DeleteSessionRequest>,
    ) -> Result<Response<app::DeleteSessionResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn send_prompt(
        &self,
        _req: Request<app::SendPromptRequest>,
    ) -> Result<Response<app::SendPromptResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn interrupt(
        &self,
        _req: Request<app::InterruptRequest>,
    ) -> Result<Response<app::InterruptResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    type StreamEventsStream = BoxStream<app::SessionEvent>;

    async fn stream_events(
        &self,
        _req: Request<app::StreamEventsRequest>,
    ) -> Result<Response<Self::StreamEventsStream>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    type ExecStream = BoxStream<app::ExecOutput>;

    async fn exec(
        &self,
        _req: Request<app::ExecRequest>,
    ) -> Result<Response<Self::ExecStream>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn get_log(
        &self,
        _req: Request<app::GetLogRequest>,
    ) -> Result<Response<app::GetLogResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn snapshot(
        &self,
        _req: Request<app::SnapshotRequest>,
    ) -> Result<Response<app::SnapshotResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn resume(
        &self,
        _req: Request<app::ResumeRequest>,
    ) -> Result<Response<app::ResumeResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn evict_local(
        &self,
        _req: Request<app::EvictLocalRequest>,
    ) -> Result<Response<app::EvictLocalResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn get_cow_state(
        &self,
        _req: Request<app::GetCowStateRequest>,
    ) -> Result<Response<app::GetCowStateResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn list_checkpoints(
        &self,
        _req: Request<app::ListCheckpointsRequest>,
    ) -> Result<Response<app::ListCheckpointsResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    type GetArtifactStream = BoxStream<app::GetArtifactResponse>;

    async fn get_artifact(
        &self,
        _req: Request<app::GetArtifactRequest>,
    ) -> Result<Response<Self::GetArtifactStream>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn create_artifact_from_path(
        &self,
        _req: Request<app::CreateArtifactFromPathRequest>,
    ) -> Result<Response<app::CreateArtifactFromPathResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }
}

pub struct AppShellRelayService {
    #[allow(dead_code)]
    pub state: SharedState,
}

#[tonic::async_trait]
impl app::shell_relay_service_server::ShellRelayService for AppShellRelayService {
    type RelayStream = BoxStream<app::RelayShellResponse>;

    async fn relay(
        &self,
        _req: Request<tonic::Streaming<app::RelayShellRequest>>,
    ) -> Result<Response<Self::RelayStream>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }
}

pub struct AppFleetService {
    #[allow(dead_code)]
    pub state: SharedState,
}

#[tonic::async_trait]
impl app::fleet_service_server::FleetService for AppFleetService {
    async fn list_hosts(
        &self,
        _req: Request<app::ListHostsRequest>,
    ) -> Result<Response<app::ListHostsResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn get_host(
        &self,
        _req: Request<app::GetHostRequest>,
    ) -> Result<Response<app::GetHostResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn get_host_cow_state(
        &self,
        _req: Request<app::GetHostCowStateRequest>,
    ) -> Result<Response<app::GetHostCowStateResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn drain_host(
        &self,
        _req: Request<app::DrainHostRequest>,
    ) -> Result<Response<app::DrainHostResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn admin_drain_host(
        &self,
        _req: Request<app::AdminDrainHostRequest>,
    ) -> Result<Response<app::AdminDrainHostResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn cordon_host(
        &self,
        _req: Request<app::CordonHostRequest>,
    ) -> Result<Response<app::CordonHostResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn uncordon_host(
        &self,
        _req: Request<app::UncordonHostRequest>,
    ) -> Result<Response<app::UncordonHostResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn get_storage_summary(
        &self,
        _req: Request<app::GetStorageSummaryRequest>,
    ) -> Result<Response<app::GetStorageSummaryResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn flush_session(
        &self,
        _req: Request<app::FlushSessionRequest>,
    ) -> Result<Response<app::FlushSessionResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn evacuate_session(
        &self,
        _req: Request<app::EvacuateSessionRequest>,
    ) -> Result<Response<app::EvacuateSessionResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn chunk_gc(
        &self,
        _req: Request<app::ChunkGcRequest>,
    ) -> Result<Response<app::ChunkGcResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn bundle_gc(
        &self,
        _req: Request<app::BundleGcRequest>,
    ) -> Result<Response<app::BundleGcResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn snapshot_blob_gc(
        &self,
        _req: Request<app::SnapshotBlobGcRequest>,
    ) -> Result<Response<app::SnapshotBlobGcResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }
}

pub struct AppImageService {
    #[allow(dead_code)]
    pub state: SharedState,
}

#[tonic::async_trait]
impl app::image_service_server::ImageService for AppImageService {
    async fn list_enabled_images(
        &self,
        _req: Request<app::ListEnabledImagesRequest>,
    ) -> Result<Response<app::ListEnabledImagesResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn enable_image(
        &self,
        _req: Request<app::EnableImageRequest>,
    ) -> Result<Response<app::EnableImageResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn disable_image(
        &self,
        _req: Request<app::DisableImageRequest>,
    ) -> Result<Response<app::DisableImageResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn refresh_image(
        &self,
        _req: Request<app::RefreshImageRequest>,
    ) -> Result<Response<app::RefreshImageResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn list_enable_jobs(
        &self,
        _req: Request<app::ListEnableJobsRequest>,
    ) -> Result<Response<app::ListEnableJobsResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn get_enable_job(
        &self,
        _req: Request<app::GetEnableJobRequest>,
    ) -> Result<Response<app::GetEnableJobResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn retry_enable_job(
        &self,
        _req: Request<app::RetryEnableJobRequest>,
    ) -> Result<Response<app::RetryEnableJobResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn list_registries(
        &self,
        _req: Request<app::ListRegistriesRequest>,
    ) -> Result<Response<app::ListRegistriesResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn add_registry(
        &self,
        _req: Request<app::AddRegistryRequest>,
    ) -> Result<Response<app::AddRegistryResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn delete_registry(
        &self,
        _req: Request<app::DeleteRegistryRequest>,
    ) -> Result<Response<app::DeleteRegistryResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }
}

pub struct AppSecretService {
    #[allow(dead_code)]
    pub state: SharedState,
}

#[tonic::async_trait]
impl app::secret_service_server::SecretService for AppSecretService {
    async fn put_secret(
        &self,
        _req: Request<app::PutSecretRequest>,
    ) -> Result<Response<app::PutSecretResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn has_secret(
        &self,
        _req: Request<app::HasSecretRequest>,
    ) -> Result<Response<app::HasSecretResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn delete_secret(
        &self,
        _req: Request<app::DeleteSecretRequest>,
    ) -> Result<Response<app::DeleteSecretResponse>, Status> {
        Err(Status::unimplemented(UNIMPLEMENTED))
    }
}
