//! `FleetService` over gRPC (ADR 0051 §2.3). Each RPC is a thin
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
        // ADR 0047: rendered from the Postgres rows alone (heartbeats persist
        // the full scheduling state) + the per-host reserved aggregate; same
        // source as the axum `hosts::list`. The removed in-memory
        // `host_registry.snapshot_state` is no longer consulted.
        let rows = self
            .state
            .services
            .meta
            .list_active_hosts()
            .await
            .map_err(|e| into_status(crate::error::ApiError::from(e)))?;
        let reserved = self
            .state
            .services
            .meta
            .per_host_reserved()
            .await
            .unwrap_or_default();
        let hosts = rows
            .into_iter()
            .map(|row| {
                let r = reserved.get(&row.id).copied().unwrap_or_default();
                let view = crate::api::hosts::HostView::from_row(row, r);
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
        let reserved = self
            .state
            .services
            .meta
            .per_host_reserved()
            .await
            .unwrap_or_default();
        let r = reserved.get(&host_id).copied().unwrap_or_default();
        let view = crate::api::hosts::HostView::from_row(row, r);
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
        // Delegates to the same helpers as api/hosts.rs::cow_state.
        // enrichment_for_session is pub(crate) — reused here instead of
        // re-inlining. No DRIFT WARNING needed: there is no copy left.
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
                Some(sid) => crate::api::hosts::enrichment_for_session(&self.state, sid).await,
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
        // ADR 0047 soft drain: the durable coordinator-side cordon
        // (`hosts.cordoned`) — heartbeats can't clobber it, every replica's
        // picker reads it from PG. Mirrors api/hosts.rs::drain; the removed
        // in-memory `host_registry` draining flag is no longer maintained.
        self.state
            .services
            .meta
            .set_host_cordoned(host_id, true)
            .await
            .map_err(|e| into_status(crate::error::ApiError::from(e)))?;
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
        // Delegates to the shared core (api/admin.rs::admin_drain_host_core).
        // SessionId → String conversion happens here at the gRPC edge.
        let result = crate::api::admin::admin_drain_host_core(&self.state, host_id)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::AdminDrainHostResponse {
            host_id: result.host_id.to_string(),
            evacuating: result.evacuating.iter().map(|s| s.to_string()).collect(),
            failures: result
                .failures
                .into_iter()
                .map(|f| app::DrainFailure {
                    session_id: f.session_id.to_string(),
                    error: f.error,
                })
                .collect(),
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
        // ADR 0047 (PG-authoritative): delegate to the shared core in
        // api/admin.rs, which writes the durable `hosts.cordoned` bit. The
        // removed in-memory `host_registry.cordon` is no longer the source
        // of truth.
        let resp = crate::api::admin::cordon_host_core(&self.state, host_id)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::CordonHostResponse {
            host_id: resp.host_id.to_string(),
            status: resp.status.to_string(),
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
        // ADR 0047 (PG-authoritative): delegate to the shared core in
        // api/admin.rs, which clears the durable `hosts.cordoned` bit.
        let resp = crate::api::admin::uncordon_host_core(&self.state, host_id)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::UncordonHostResponse {
            host_id: resp.host_id.to_string(),
            status: resp.status.to_string(),
        }))
    }

    // Faithful gRPC port of the old `DELETE /api/admin/hosts/:id` handler
    // (ADR 0048; dropped in the ADR 0051 gRPC-only migration but never
    // re-added, which stranded the operator's scale-down wave). 204 →
    // empty Ok; the "still bound" 409 → FAILED_PRECONDITION; other errors
    // → INTERNAL.
    async fn delete_host(
        &self,
        req: Request<app::DeleteHostRequest>,
    ) -> Result<Response<app::DeleteHostResponse>, Status> {
        self.auth.check(&req)?;
        let host_id: engram_core::HostId = req
            .get_ref()
            .host_id
            .parse()
            .map_err(|_| Status::invalid_argument("malformed host_id"))?;
        use engram_core::types::session::DeleteHostOutcome;
        match self.state.services.meta.delete_host(host_id).await {
            Ok(DeleteHostOutcome::Deleted) => {
                // Drop the in-memory routing entry (best-effort; a sibling
                // replica clears its own on the host_dead notify / TTL).
                self.state.host_registry.unregister(host_id);
                tracing::info!(%host_id, "admin: host deregistered (row deleted)");
                Ok(Response::new(app::DeleteHostResponse {}))
            }
            Ok(DeleteHostOutcome::SessionsBound(n)) => Err(Status::failed_precondition(format!(
                "host {host_id} still has {n} bound session(s); drain it before deleting"
            ))),
            Err(e) => Err(Status::internal(format!("delete_host: {e}"))),
        }
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

    async fn get_fleet_demand(
        &self,
        req: Request<app::GetFleetDemandRequest>,
    ) -> Result<Response<app::GetFleetDemandResponse>, Status> {
        self.auth.check(&req)?;
        let d = crate::api::admin::fleet_demand_core(&self.state).await;
        Ok(Response::new(app::GetFleetDemandResponse {
            ready_hosts: d.ready_hosts,
            schedulable_hosts: d.schedulable_hosts,
            free_mib: d.free_mib,
            total_mib: d.total_mib,
            free_vcpus: d.free_vcpus,
            total_vcpus: d.total_vcpus,
            cordoned_hosts: d.cordoned_hosts,
            queued_sessions: d.queued_sessions,
            queued_mib: d.queued_mib,
            queued_vcpus: d.queued_vcpus,
        }))
    }
}
