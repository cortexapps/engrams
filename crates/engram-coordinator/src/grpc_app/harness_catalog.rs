//! `HarnessCatalogService` over app-gRPC (ADR 0062): the harness catalog — the
//! registry of selectable agent harnesses (built-in + custom, all OCI
//! artifacts). `RegisterHarness` pulls the OCI artifact, validates its
//! `harness.toml`, publishes the extracted tree, upserts the row, and re-packs
//! the single catalog squashfs (every live harness) that mounts on `dyn_0`
//! (see [`crate::harness_catalog`]). List/Get project the stored descriptor;
//! Delete soft-deletes + re-packs.

use std::sync::Arc;

use engram_protocol::app;
use tonic::{Request, Response, Status};

use super::{auth, into_status};
use crate::error::ApiError;
use crate::state::SharedState;

pub struct AppHarnessCatalogService {
    pub state: SharedState,
    pub auth: Arc<auth::BearerAuth>,
}

/// engram-core `HarnessDescriptor` → proto. All env values are config (model
/// ids, flags) — never secrets — so the projection is safe on the wire. The
/// launch contract (`exec`/`args`) is coordinator-internal and omitted.
fn descriptor_to_proto(
    d: &engram_core::types::harness::HarnessDescriptor,
) -> app::HarnessDescriptor {
    app::HarnessDescriptor {
        name: d.name.clone(),
        label: d.label.clone(),
        auth: Some(app::HarnessAuth {
            org_env: d.auth.org_env.clone(),
            user_env: d.auth.user_env.clone(),
        }),
        models: d.models.iter().map(option_to_proto).collect(),
        effort: d.effort.iter().map(option_to_proto).collect(),
    }
}

fn option_to_proto(o: &engram_core::types::harness::HarnessOption) -> app::HarnessOption {
    app::HarnessOption {
        id: o.id.clone(),
        label: o.label.clone(),
        default: o.default,
        env: o.env.clone().into_iter().collect(),
    }
}

/// A catalog row → `HarnessSummary` (name + projected descriptor). A row whose
/// stored `harness.toml` fails to parse keeps its name with an empty descriptor
/// (so an operator can still see + delete it) rather than being dropped.
fn harness_to_summary(h: &engram_core::types::CatalogHarness) -> app::HarnessSummary {
    let descriptor = h.descriptor().ok().as_ref().map(descriptor_to_proto);
    app::HarnessSummary {
        name: h.name.clone(),
        descriptor,
    }
}

// EVERY RPC body starts with self.auth.check(&req)? — see auth.rs and the convention test.
#[tonic::async_trait]
impl app::harness_catalog_service_server::HarnessCatalogService for AppHarnessCatalogService {
    async fn register_harness(
        &self,
        req: Request<app::RegisterHarnessRequest>,
    ) -> Result<Response<app::RegisterHarnessResponse>, Status> {
        self.auth.check(&req)?;
        let app::RegisterHarnessRequest {
            name,
            oci_ref,
            owner,
        } = req.into_inner();
        let row = crate::harness_catalog::register_harness(&self.state, &name, &oci_ref, &owner)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::RegisterHarnessResponse {
            harness: Some(harness_to_summary(&row)),
        }))
    }

    async fn list_harnesses(
        &self,
        req: Request<app::ListHarnessesRequest>,
    ) -> Result<Response<app::ListHarnessesResponse>, Status> {
        self.auth.check(&req)?;
        let rows = self
            .state
            .services
            .meta
            .list_harnesses()
            .await
            .map_err(|e| into_status(ApiError::from(e)))?;
        Ok(Response::new(app::ListHarnessesResponse {
            harnesses: rows.iter().map(harness_to_summary).collect(),
        }))
    }

    async fn get_harness(
        &self,
        req: Request<app::GetHarnessRequest>,
    ) -> Result<Response<app::GetHarnessResponse>, Status> {
        self.auth.check(&req)?;
        let name = req.into_inner().name;
        let row = self
            .state
            .services
            .meta
            .get_harness_by_name(&name)
            .await
            .map_err(|e| into_status(ApiError::from(e)))?
            .ok_or_else(|| {
                into_status(ApiError::NotFound(format!("harness `{name}` not found")))
            })?;
        Ok(Response::new(app::GetHarnessResponse {
            harness: Some(harness_to_summary(&row)),
        }))
    }

    async fn delete_harness(
        &self,
        req: Request<app::DeleteHarnessRequest>,
    ) -> Result<Response<app::DeleteHarnessResponse>, Status> {
        self.auth.check(&req)?;
        let name = req.into_inner().name;
        let deleted = crate::harness_catalog::delete_harness(&self.state, &name)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::DeleteHarnessResponse { deleted }))
    }
}
