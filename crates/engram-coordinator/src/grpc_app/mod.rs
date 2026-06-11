//! Orchestrator-facing app gRPC surface (ADR 0039 §2.3). Lives beside
//! the axum API during the migration; the axum web routes retire in
//! Phase 5.
//!
//! Task 8 scaffold: every RPC answers `Code::Unimplemented`. Tasks
//! 10-13 replace the stubs service-by-service with extract-and-delegate
//! implementations over the same `AppState` the axum handlers use.
//! The caller is a single trusted service (the orchestrator),
//! authenticated per-RPC by a static bearer ([`auth::BearerAuth`],
//! ADR 0039 §5 — fail closed when no tokens are configured); per-user
//! authz lives over there, not here.

pub mod auth;

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use engram_protocol::app;
use tonic::{Request, Response, Status};

use crate::state::SharedState;

/// Boxed response stream for the server-streaming RPCs. The concrete
/// streams arrive with the real implementations (Tasks 11-12); the
/// stubs only need the associated types to satisfy the traits.
type BoxStream<T> = Pin<Box<dyn tokio_stream::Stream<Item = Result<T, Status>> + Send>>;

const UNIMPLEMENTED: &str = "ADR 0039 phase 2";

/// ADR 0039 §9.4: keepalive PINGs so a dead orchestrator's streams
/// are detected and torn down (releasing leases/subscriptions)
/// instead of leaking until TCP gives up.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the tonic server for the app surface: all five services on
/// one listener, HTTP/2 keepalives per [`KEEPALIVE_INTERVAL`] /
/// [`KEEPALIVE_TIMEOUT`].
///
/// Returns the un-bound router; the caller binds the listener and
/// picks the serve strategy (`serve_with_incoming_shutdown` over an
/// eagerly bound listener in lib.rs, `serve_with_incoming` on an
/// ephemeral port in tests).
pub fn server(state: SharedState) -> tonic::transport::server::Router {
    // One shared allow-list snapshot for all five services, taken from
    // config at construction time (no hot-reload; rotation = overlap
    // both tokens, restart, drop the old one).
    let auth = Arc::new(auth::BearerAuth::new(state.cfg.app_grpc_tokens.clone()));
    tonic::transport::Server::builder()
        .http2_keepalive_interval(Some(KEEPALIVE_INTERVAL))
        .http2_keepalive_timeout(Some(KEEPALIVE_TIMEOUT))
        .add_service(app::session_service_server::SessionServiceServer::new(
            AppSessionService {
                state: state.clone(),
                auth: auth.clone(),
            },
        ))
        .add_service(
            app::shell_relay_service_server::ShellRelayServiceServer::new(AppShellRelayService {
                state: state.clone(),
                auth: auth.clone(),
            }),
        )
        .add_service(app::fleet_service_server::FleetServiceServer::new(
            AppFleetService {
                state: state.clone(),
                auth: auth.clone(),
            },
        ))
        .add_service(app::image_service_server::ImageServiceServer::new(
            AppImageService {
                state: state.clone(),
                auth: auth.clone(),
            },
        ))
        .add_service(app::secret_service_server::SecretServiceServer::new(
            AppSecretService { state, auth },
        ))
}

pub struct AppSessionService {
    // Consumed from Task 10 onward; the scaffold only carries it.
    #[allow(dead_code)]
    pub state: SharedState,
    pub auth: Arc<auth::BearerAuth>,
}

// EVERY RPC body starts with self.auth.check(&req)? — see auth.rs and the convention test.
#[tonic::async_trait]
impl app::session_service_server::SessionService for AppSessionService {
    async fn list_sessions(
        &self,
        req: Request<app::ListSessionsRequest>,
    ) -> Result<Response<app::ListSessionsResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn create_session(
        &self,
        req: Request<app::CreateSessionRequest>,
    ) -> Result<Response<app::CreateSessionResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn get_session(
        &self,
        req: Request<app::GetSessionRequest>,
    ) -> Result<Response<app::GetSessionResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn delete_session(
        &self,
        req: Request<app::DeleteSessionRequest>,
    ) -> Result<Response<app::DeleteSessionResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn send_prompt(
        &self,
        req: Request<app::SendPromptRequest>,
    ) -> Result<Response<app::SendPromptResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn interrupt(
        &self,
        req: Request<app::InterruptRequest>,
    ) -> Result<Response<app::InterruptResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    type StreamEventsStream = BoxStream<app::SessionEvent>;

    async fn stream_events(
        &self,
        req: Request<app::StreamEventsRequest>,
    ) -> Result<Response<Self::StreamEventsStream>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    type ExecStream = BoxStream<app::ExecOutput>;

    async fn exec(
        &self,
        req: Request<app::ExecRequest>,
    ) -> Result<Response<Self::ExecStream>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn get_log(
        &self,
        req: Request<app::GetLogRequest>,
    ) -> Result<Response<app::GetLogResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn snapshot(
        &self,
        req: Request<app::SnapshotRequest>,
    ) -> Result<Response<app::SnapshotResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn resume(
        &self,
        req: Request<app::ResumeRequest>,
    ) -> Result<Response<app::ResumeResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn evict_local(
        &self,
        req: Request<app::EvictLocalRequest>,
    ) -> Result<Response<app::EvictLocalResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn get_cow_state(
        &self,
        req: Request<app::GetCowStateRequest>,
    ) -> Result<Response<app::GetCowStateResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn list_checkpoints(
        &self,
        req: Request<app::ListCheckpointsRequest>,
    ) -> Result<Response<app::ListCheckpointsResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    type GetArtifactStream = BoxStream<app::GetArtifactResponse>;

    async fn get_artifact(
        &self,
        req: Request<app::GetArtifactRequest>,
    ) -> Result<Response<Self::GetArtifactStream>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn create_artifact_from_path(
        &self,
        req: Request<app::CreateArtifactFromPathRequest>,
    ) -> Result<Response<app::CreateArtifactFromPathResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }
}

pub struct AppShellRelayService {
    #[allow(dead_code)]
    pub state: SharedState,
    pub auth: Arc<auth::BearerAuth>,
}

// EVERY RPC body starts with self.auth.check(&req)? — see auth.rs and the convention test.
#[tonic::async_trait]
impl app::shell_relay_service_server::ShellRelayService for AppShellRelayService {
    type RelayStream = BoxStream<app::RelayShellResponse>;

    async fn relay(
        &self,
        req: Request<tonic::Streaming<app::RelayShellRequest>>,
    ) -> Result<Response<Self::RelayStream>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }
}

pub struct AppFleetService {
    #[allow(dead_code)]
    pub state: SharedState,
    pub auth: Arc<auth::BearerAuth>,
}

// EVERY RPC body starts with self.auth.check(&req)? — see auth.rs and the convention test.
#[tonic::async_trait]
impl app::fleet_service_server::FleetService for AppFleetService {
    async fn list_hosts(
        &self,
        req: Request<app::ListHostsRequest>,
    ) -> Result<Response<app::ListHostsResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn get_host(
        &self,
        req: Request<app::GetHostRequest>,
    ) -> Result<Response<app::GetHostResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn get_host_cow_state(
        &self,
        req: Request<app::GetHostCowStateRequest>,
    ) -> Result<Response<app::GetHostCowStateResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn drain_host(
        &self,
        req: Request<app::DrainHostRequest>,
    ) -> Result<Response<app::DrainHostResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn admin_drain_host(
        &self,
        req: Request<app::AdminDrainHostRequest>,
    ) -> Result<Response<app::AdminDrainHostResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn cordon_host(
        &self,
        req: Request<app::CordonHostRequest>,
    ) -> Result<Response<app::CordonHostResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn uncordon_host(
        &self,
        req: Request<app::UncordonHostRequest>,
    ) -> Result<Response<app::UncordonHostResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn get_storage_summary(
        &self,
        req: Request<app::GetStorageSummaryRequest>,
    ) -> Result<Response<app::GetStorageSummaryResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn flush_session(
        &self,
        req: Request<app::FlushSessionRequest>,
    ) -> Result<Response<app::FlushSessionResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn evacuate_session(
        &self,
        req: Request<app::EvacuateSessionRequest>,
    ) -> Result<Response<app::EvacuateSessionResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn chunk_gc(
        &self,
        req: Request<app::ChunkGcRequest>,
    ) -> Result<Response<app::ChunkGcResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn bundle_gc(
        &self,
        req: Request<app::BundleGcRequest>,
    ) -> Result<Response<app::BundleGcResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn snapshot_blob_gc(
        &self,
        req: Request<app::SnapshotBlobGcRequest>,
    ) -> Result<Response<app::SnapshotBlobGcResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }
}

pub struct AppImageService {
    #[allow(dead_code)]
    pub state: SharedState,
    pub auth: Arc<auth::BearerAuth>,
}

// EVERY RPC body starts with self.auth.check(&req)? — see auth.rs and the convention test.
#[tonic::async_trait]
impl app::image_service_server::ImageService for AppImageService {
    async fn list_enabled_images(
        &self,
        req: Request<app::ListEnabledImagesRequest>,
    ) -> Result<Response<app::ListEnabledImagesResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn enable_image(
        &self,
        req: Request<app::EnableImageRequest>,
    ) -> Result<Response<app::EnableImageResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn disable_image(
        &self,
        req: Request<app::DisableImageRequest>,
    ) -> Result<Response<app::DisableImageResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn refresh_image(
        &self,
        req: Request<app::RefreshImageRequest>,
    ) -> Result<Response<app::RefreshImageResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn list_enable_jobs(
        &self,
        req: Request<app::ListEnableJobsRequest>,
    ) -> Result<Response<app::ListEnableJobsResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn get_enable_job(
        &self,
        req: Request<app::GetEnableJobRequest>,
    ) -> Result<Response<app::GetEnableJobResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn retry_enable_job(
        &self,
        req: Request<app::RetryEnableJobRequest>,
    ) -> Result<Response<app::RetryEnableJobResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn list_registries(
        &self,
        req: Request<app::ListRegistriesRequest>,
    ) -> Result<Response<app::ListRegistriesResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn add_registry(
        &self,
        req: Request<app::AddRegistryRequest>,
    ) -> Result<Response<app::AddRegistryResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn delete_registry(
        &self,
        req: Request<app::DeleteRegistryRequest>,
    ) -> Result<Response<app::DeleteRegistryResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }
}

pub struct AppSecretService {
    #[allow(dead_code)]
    pub state: SharedState,
    pub auth: Arc<auth::BearerAuth>,
}

// EVERY RPC body starts with self.auth.check(&req)? — see auth.rs and the convention test.
#[tonic::async_trait]
impl app::secret_service_server::SecretService for AppSecretService {
    async fn put_secret(
        &self,
        req: Request<app::PutSecretRequest>,
    ) -> Result<Response<app::PutSecretResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn has_secret(
        &self,
        req: Request<app::HasSecretRequest>,
    ) -> Result<Response<app::HasSecretResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }

    async fn delete_secret(
        &self,
        req: Request<app::DeleteSecretRequest>,
    ) -> Result<Response<app::DeleteSecretResponse>, Status> {
        self.auth.check(&req)?;
        Err(Status::unimplemented(UNIMPLEMENTED))
    }
}

#[cfg(test)]
mod convention {
    //! Source-scan guard for the fail-closed auth convention (ADR 0039
    //! §5): EVERY app-gRPC RPC body must begin with `self.auth.check(&req)?`
    //! before it does anything else. The five service impls hand-roll
    //! that line in each method (no tower layer — see `auth::BearerAuth`),
    //! so nothing structural stops a future RPC from silently skipping it.
    //! This test counts, in the source, one auth check per `async fn` and
    //! fails loudly if they ever diverge.
    //!
    //! ==========================================================
    //! IMPLEMENTERS, READ THIS — Tasks 10-13 rewrite all 43 stub
    //! bodies and WILL split the services into per-file modules.
    //! When you add a new file under `src/grpc_app/` that holds RPC
    //! `async fn`s, ADD IT to `SOURCES` below (one `include_str!`
    //! line per file). The scan only sees files listed here; a new
    //! service file that isn't listed is invisible to this guard and
    //! its missing auth checks will NOT be caught.
    //! ==========================================================

    /// Every grpc_app source file that contains RPC `async fn`s. Each
    /// entry is `include_str!`'d and scanned. Append new per-service
    /// files here as Tasks 10-13 split them out of `mod.rs`.
    const SOURCES: &[(&str, &str)] = &[("mod.rs", include_str!("mod.rs"))];

    /// The auth line that must open every RPC body. The trailing `;` is
    /// load-bearing: it distinguishes a real call site from the prose
    /// banner comment above each impl block (which writes the same
    /// expression followed by ` — see auth.rs …`, no semicolon) so the
    /// banners don't inflate the count.
    const AUTH_CHECK: &str = "self.auth.check(&req)?;";

    /// Marker that begins this test module. We scan only the source
    /// *above* it so the literals in this very file (the `AUTH_CHECK`
    /// const, the failure messages, the `async fn` in any future test
    /// helpers) don't pollute the counts.
    const TEST_MODULE_MARKER: &str = "#[cfg(test)]\nmod convention";

    #[test]
    fn every_rpc_starts_with_an_auth_check() {
        for (name, full_src) in SOURCES {
            // Scan only the non-test portion of each file.
            let src = match full_src.find(TEST_MODULE_MARKER) {
                Some(idx) => &full_src[..idx],
                None => full_src,
            };

            let rpc_count = src.matches("async fn ").count();
            let check_count = src.matches(AUTH_CHECK).count();

            assert_eq!(
                check_count, rpc_count,
                "AUTH CONVENTION VIOLATED in src/grpc_app/{name}: found {rpc_count} \
                 `async fn ` RPC method(s) but {check_count} `{AUTH_CHECK}` call(s). \
                 Every app-gRPC RPC body MUST begin with `{AUTH_CHECK}` before any \
                 other logic (ADR 0039 §5, fail-closed machine auth — see \
                 src/grpc_app/auth.rs). Open src/grpc_app/{name}, find the RPC `async fn` \
                 that is missing the leading `{AUTH_CHECK}`, and add it. If you added a \
                 NEW per-service file, also add it to the `SOURCES` list in this test \
                 (src/grpc_app/mod.rs, mod convention)."
            );
        }
    }
}
