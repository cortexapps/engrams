//! `FleetService` over gRPC (ADR 0039 §2.3). Each RPC is a thin
//! transport adapter: auth-check, decode, delegate to the same axum
//! handlers (or their inner logic) that serve the REST surface, encode
//! the response. No admin gating — the caller (orchestrator) is trusted;
//! per-user authz lives over there (ADR §6).

use std::sync::Arc;

use engram_protocol::app;
use tonic::{Request, Response, Status};

use super::{auth, convert, into_status, parse_session_id};
use crate::state::SharedState;

pub struct AppFleetService {
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
        let rows = self
            .state
            .services
            .meta
            .list_active_hosts()
            .await
            .map_err(|e| into_status(crate::error::ApiError::from(e)))?;
        let hosts = rows
            .into_iter()
            .map(|row| {
                let live = self.state.host_registry.snapshot_state(row.id);
                let view = crate::api::hosts::HostView::from_row_and_live(row, live);
                convert::host_view_to_proto(&view)
            })
            .collect();
        Ok(Response::new(app::ListHostsResponse { hosts }))
    }

    async fn get_host(
        &self,
        req: Request<app::GetHostRequest>,
    ) -> Result<Response<app::GetHostResponse>, Status> {
        self.auth.check(&req)?;
        let host_id: engram_core::HostId = req
            .get_ref()
            .host_id
            .parse()
            .map_err(|_| Status::invalid_argument("malformed host_id"))?;
        let rows = self
            .state
            .services
            .meta
            .list_active_hosts()
            .await
            .map_err(|e| into_status(crate::error::ApiError::from(e)))?;
        let row = rows.into_iter().find(|r| r.id == host_id).ok_or_else(|| {
            into_status(crate::error::ApiError::NotFound("host not found".into()))
        })?;
        let live = self.state.host_registry.snapshot_state(host_id);
        let view = crate::api::hosts::HostView::from_row_and_live(row, live);
        Ok(Response::new(app::GetHostResponse {
            host: Some(convert::host_view_to_proto(&view)),
        }))
    }

    async fn get_host_cow_state(
        &self,
        req: Request<app::GetHostCowStateRequest>,
    ) -> Result<Response<app::GetHostCowStateResponse>, Status> {
        self.auth.check(&req)?;
        let host_id: engram_core::HostId = req
            .get_ref()
            .host_id
            .parse()
            .map_err(|_| Status::invalid_argument("malformed host_id"))?;
        // DRIFT WARNING: this replicates the logic from api/hosts.rs::cow_state
        // (HostCowStateResponse assembly). If that handler changes, this must
        // change too. Back-pointer: api/hosts.rs::cow_state.
        let backend = self
            .state
            .host_registry
            .backend_of(host_id)
            .ok_or_else(|| {
                into_status(crate::error::ApiError::NotFound(
                    "host not registered".into(),
                ))
            })?;
        let records =
            crate::cow_state::fetch_for_host(&self.state.cow_state_cache, host_id, backend)
                .await
                .map_err(|e| {
                    into_status(crate::error::ApiError::Internal(format!("cow_state: {e}")))
                })?;
        let assignments = self
            .state
            .services
            .meta
            .list_active_sandbox_assignments_on_host(host_id)
            .await
            .map_err(|e| into_status(crate::error::ApiError::from(e)))?;
        let session_for: std::collections::HashMap<engram_core::SandboxId, engram_core::SessionId> =
            assignments.into_iter().map(|(sid, sb)| (sb, sid)).collect();
        let mut sessions = Vec::with_capacity(records.len());
        for record in records {
            let session_id = session_for.get(&record.sandbox_id).copied();
            let (memory_manifest, last_snapshot_at) = match session_id {
                Some(sid) => {
                    match self
                        .state
                        .services
                        .meta
                        .latest_snapshot_for_session(sid)
                        .await
                    {
                        Ok(Some(rec)) => (rec.memory_manifest, Some(rec.created_at)),
                        Ok(None) => (None, None),
                        Err(e) => {
                            tracing::debug!(%sid, error = %e, "get_host_cow_state: snapshot lookup failed");
                            (None, None)
                        }
                    }
                }
                None => (None, None),
            };
            let view = crate::cow_state::CowStateView::from_record(
                &record,
                session_id,
                memory_manifest,
                last_snapshot_at,
            );
            sessions.push(convert::cow_state_to_proto(&view));
        }
        Ok(Response::new(app::GetHostCowStateResponse {
            host_id: host_id.to_string(),
            sessions,
        }))
    }

    async fn drain_host(
        &self,
        req: Request<app::DrainHostRequest>,
    ) -> Result<Response<app::DrainHostResponse>, Status> {
        self.auth.check(&req)?;
        let host_id: engram_core::HostId = req
            .get_ref()
            .host_id
            .parse()
            .map_err(|_| Status::invalid_argument("malformed host_id"))?;
        // Soft drain: mirrors api/hosts.rs::drain (POST /hosts/:id/drain).
        self.state
            .services
            .meta
            .set_host_status(host_id, engram_core::types::HostStatus::Draining)
            .await
            .map_err(|e| into_status(crate::error::ApiError::from(e)))?;
        if let Some(mut s) = self.state.host_registry.snapshot_state(host_id) {
            s.draining = true;
            self.state.host_registry.update_state(host_id, s);
        }
        Ok(Response::new(app::DrainHostResponse {}))
    }

    async fn admin_drain_host(
        &self,
        req: Request<app::AdminDrainHostRequest>,
    ) -> Result<Response<app::AdminDrainHostResponse>, Status> {
        self.auth.check(&req)?;
        let host_id: engram_core::HostId = req
            .get_ref()
            .host_id
            .parse()
            .map_err(|_| Status::invalid_argument("malformed host_id"))?;
        // Admin drain: delegates to the axum admin handler logic.
        // DRIFT WARNING: this replicates api/admin.rs::drain_host.
        // Back-pointer: api/admin.rs::drain_host.
        if !self.state.host_registry.cordon(host_id) {
            return Err(into_status(crate::error::ApiError::NotFound(format!(
                "host {host_id} not registered"
            ))));
        }
        if let Err(e) = self
            .state
            .services
            .meta
            .set_host_status(host_id, engram_core::types::HostStatus::Draining)
            .await
        {
            tracing::warn!(%host_id, error = %e, "admin_drain_host: PG write failed; in-memory flag set");
        }
        let assignments = self
            .state
            .services
            .meta
            .list_active_sandbox_assignments_on_host(host_id)
            .await
            .map_err(|e| {
                into_status(crate::error::ApiError::Internal(format!(
                    "drain: list sessions: {e}"
                )))
            })?;
        if assignments.is_empty() {
            return Ok(Response::new(app::AdminDrainHostResponse {
                host_id: host_id.to_string(),
                evacuating: Vec::new(),
                failures: Vec::new(),
            }));
        }
        let mut tasks = tokio::task::JoinSet::new();
        for (session_id, sandbox_id) in &assignments {
            let st = self.state.clone();
            let sid = *session_id;
            let sb = *sandbox_id;
            tasks.spawn(async move {
                let outcome = crate::idle_evictor::evict_session_to_state(
                    &st,
                    sid,
                    sb,
                    engram_core::types::SessionState::Evacuating,
                )
                .await;
                (sid, outcome)
            });
        }
        let mut evacuating: Vec<String> = Vec::new();
        let mut failures: Vec<app::DrainFailure> = Vec::new();
        while let Some(join) = tasks.join_next().await {
            match join {
                Ok((sid, Ok(()))) => evacuating.push(sid.to_string()),
                Ok((sid, Err(e))) => {
                    tracing::warn!(%sid, %host_id, error = %e, "admin_drain_host: per-session evict failed");
                    failures.push(app::DrainFailure {
                        session_id: sid.to_string(),
                        error: e.to_string(),
                    });
                }
                Err(e) => {
                    tracing::warn!(%host_id, error = %e, "admin_drain_host: join error");
                }
            }
        }
        Ok(Response::new(app::AdminDrainHostResponse {
            host_id: host_id.to_string(),
            evacuating,
            failures,
        }))
    }

    async fn cordon_host(
        &self,
        req: Request<app::CordonHostRequest>,
    ) -> Result<Response<app::CordonHostResponse>, Status> {
        self.auth.check(&req)?;
        let host_id: engram_core::HostId = req
            .get_ref()
            .host_id
            .parse()
            .map_err(|_| Status::invalid_argument("malformed host_id"))?;
        if !self.state.host_registry.cordon(host_id) {
            return Err(into_status(crate::error::ApiError::NotFound(format!(
                "host {host_id} not registered"
            ))));
        }
        if let Err(e) = self
            .state
            .services
            .meta
            .set_host_status(host_id, engram_core::types::HostStatus::Draining)
            .await
        {
            tracing::warn!(%host_id, error = %e, "cordon_host: PG write failed; in-memory flag set");
        }
        Ok(Response::new(app::CordonHostResponse {
            host_id: host_id.to_string(),
            status: "draining".to_string(),
        }))
    }

    async fn uncordon_host(
        &self,
        req: Request<app::UncordonHostRequest>,
    ) -> Result<Response<app::UncordonHostResponse>, Status> {
        self.auth.check(&req)?;
        let host_id: engram_core::HostId = req
            .get_ref()
            .host_id
            .parse()
            .map_err(|_| Status::invalid_argument("malformed host_id"))?;
        if !self.state.host_registry.uncordon(host_id) {
            return Err(into_status(crate::error::ApiError::NotFound(format!(
                "host {host_id} not registered"
            ))));
        }
        if let Err(e) = self
            .state
            .services
            .meta
            .set_host_status(host_id, engram_core::types::HostStatus::Ready)
            .await
        {
            tracing::warn!(%host_id, error = %e, "uncordon_host: PG write failed; in-memory flag set");
        }
        Ok(Response::new(app::UncordonHostResponse {
            host_id: host_id.to_string(),
            status: "ready".to_string(),
        }))
    }

    async fn get_storage_summary(
        &self,
        req: Request<app::GetStorageSummaryRequest>,
    ) -> Result<Response<app::GetStorageSummaryResponse>, Status> {
        self.auth.check(&req)?;
        let resp = crate::api::storage::storage_summary_core(&self.state)
            .await
            .map_err(into_status)?;
        Ok(Response::new(convert::storage_summary_to_proto(resp)))
    }

    async fn flush_session(
        &self,
        req: Request<app::FlushSessionRequest>,
    ) -> Result<Response<app::FlushSessionResponse>, Status> {
        self.auth.check(&req)?;
        let id = parse_session_id(&req.get_ref().session_id)?;
        let result = crate::api::admin::flush_now_core(&self.state, id)
            .await
            .map_err(into_status)?;
        Ok(Response::new(convert::flush_now_result_to_proto(result)))
    }

    async fn evacuate_session(
        &self,
        req: Request<app::EvacuateSessionRequest>,
    ) -> Result<Response<app::EvacuateSessionResponse>, Status> {
        self.auth.check(&req)?;
        let id = parse_session_id(&req.get_ref().session_id)?;
        // `target_host` is ignored per the proto comment (future override).
        let result = crate::api::admin::evacuate_session_core(&self.state, id)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::EvacuateSessionResponse {
            session_id: result.session_id.to_string(),
            status: result.status.to_string(),
        }))
    }

    async fn chunk_gc(
        &self,
        req: Request<app::ChunkGcRequest>,
    ) -> Result<Response<app::ChunkGcResponse>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        let result = crate::api::admin::chunk_gc_core(&self.state, r.dry_run, r.grace_secs)
            .await
            .map_err(into_status)?;
        Ok(Response::new(convert::chunk_gc_result_to_proto(result)))
    }

    async fn bundle_gc(
        &self,
        req: Request<app::BundleGcRequest>,
    ) -> Result<Response<app::BundleGcResponse>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        let result = crate::api::admin::bundle_gc_core(&self.state, r.dry_run, r.grace_secs)
            .await
            .map_err(into_status)?;
        Ok(Response::new(convert::bundle_gc_result_to_proto(result)))
    }

    async fn snapshot_blob_gc(
        &self,
        req: Request<app::SnapshotBlobGcRequest>,
    ) -> Result<Response<app::SnapshotBlobGcResponse>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        let result = crate::api::admin::snapshot_blob_gc_core(&self.state, r.dry_run, r.grace_secs)
            .await
            .map_err(into_status)?;
        Ok(Response::new(convert::snapshot_blob_gc_result_to_proto(
            result,
        )))
    }
}
