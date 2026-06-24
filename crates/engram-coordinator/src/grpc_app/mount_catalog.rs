//! `MountCatalogService` over app-gRPC (ADR 0055 P2): the org-shared
//! user-uploaded skill catalog. The caller is the trusted orchestrator
//! (bearer-authed); per-user authz lives there, and the `owner` carried on
//! RegisterSkill is attribution / GC ownership only.
//!
//! `RegisterSkill` packs the upload into a deterministic content-addressed
//! squashfs ([`crate::skill_pack`]), publishes it to BlobStorage, and upserts the
//! catalog row — so the sha enters `bundle_pin_set()` and every host stages it
//! via the existing materialize-by-sha path. List/Get/Delete are thin reads +
//! soft-delete (soft-delete → the existing bundle GC reclaims the blob).

use std::sync::Arc;

use engram_protocol::app;
use tonic::{Request, Response, Status};

use super::{auth, into_status};
use crate::error::ApiError;
use crate::skill_pack::{self, PackError};
use crate::state::SharedState;

pub struct AppMountCatalogService {
    pub state: SharedState,
    pub auth: Arc<auth::BearerAuth>,
}

fn skill_to_proto(s: engram_core::types::CatalogSkill) -> app::CatalogSkill {
    app::CatalogSkill {
        id: s.id,
        owner: s.owner,
        name: s.name,
        description: s.description,
        sha256: s.sha256,
        size_bytes: s.size_bytes,
        created_at: s.created_at.to_rfc3339(),
    }
}

#[allow(clippy::result_large_err)] // tonic::Status is the RPC error type (see into_status)
fn pack_to_status(e: PackError) -> Status {
    match e {
        PackError::Invalid(m) => into_status(ApiError::BadRequest(m)),
        PackError::Internal(m) => into_status(ApiError::Internal(m)),
    }
}

// EVERY RPC body starts with self.auth.check(&req)? — see auth.rs and the convention test.
#[tonic::async_trait]
impl app::mount_catalog_service_server::MountCatalogService for AppMountCatalogService {
    async fn register_skill(
        &self,
        req: Request<app::RegisterSkillRequest>,
    ) -> Result<Response<app::RegisterSkillResponse>, Status> {
        self.auth.check(&req)?;
        let app::RegisterSkillRequest {
            name,
            description,
            owner,
            payload_tar,
            bins,
        } = req.into_inner();
        let name = name.trim().to_string();

        // A catalog name may not shadow a fleet (baked admin) bundle — the
        // resolver treats `fleet_stamp ∪ catalog` as one flat name namespace.
        let fleet = crate::api::sessions::fleet_bundle_catalog(&self.state)
            .await
            .map_err(into_status)?;
        if fleet.contains_key(&name) {
            return Err(into_status(ApiError::Conflict(format!(
                "skill name `{name}` collides with a built-in fleet bundle"
            ))));
        }

        // Pack into a content-addressed squashfs (validates name + SKILL.md +
        // size + declared bins; shells to mksquashfs).
        let packed = skill_pack::pack_skill(&name, &payload_tar, &bins).map_err(pack_to_status)?;

        // Publish the squashfs (content-addressed; re-registering identical bytes
        // overwrites the same key — idempotent).
        let key = engram_core::types::sandbox::AuxRoDrive::blob_key(&packed.sha256);
        self.state
            .services
            .blob
            .put(&key, packed.squashfs.into())
            .await
            .map_err(|e| into_status(ApiError::Internal(format!("publish skill blob: {e}"))))?;

        // Upsert the catalog row (its sha now folds into bundle_pin_set()).
        let row = self
            .state
            .services
            .meta
            .register_skill(
                owner.trim(),
                &name,
                description.trim(),
                &packed.sha256,
                &packed.mount_json,
                packed.size_bytes,
            )
            .await
            .map_err(|e| into_status(ApiError::from(e)))?;

        tracing::info!(
            skill = %row.name,
            sha256 = %row.sha256,
            size_bytes = row.size_bytes,
            owner = %row.owner,
            "registered user-uploaded skill",
        );
        Ok(Response::new(app::RegisterSkillResponse {
            skill: Some(skill_to_proto(row)),
        }))
    }

    async fn list_skills(
        &self,
        req: Request<app::ListSkillsRequest>,
    ) -> Result<Response<app::ListSkillsResponse>, Status> {
        self.auth.check(&req)?;
        let rows = self
            .state
            .services
            .meta
            .list_skills()
            .await
            .map_err(|e| into_status(ApiError::from(e)))?;
        Ok(Response::new(app::ListSkillsResponse {
            skills: rows.into_iter().map(skill_to_proto).collect(),
        }))
    }

    async fn get_skill(
        &self,
        req: Request<app::GetSkillRequest>,
    ) -> Result<Response<app::GetSkillResponse>, Status> {
        self.auth.check(&req)?;
        let name = req.into_inner().name;
        let row = self
            .state
            .services
            .meta
            .get_skill_by_name(&name)
            .await
            .map_err(|e| into_status(ApiError::from(e)))?
            .ok_or_else(|| into_status(ApiError::NotFound(format!("skill `{name}` not found"))))?;
        Ok(Response::new(app::GetSkillResponse {
            skill: Some(skill_to_proto(row)),
        }))
    }

    async fn delete_skill(
        &self,
        req: Request<app::DeleteSkillRequest>,
    ) -> Result<Response<app::DeleteSkillResponse>, Status> {
        self.auth.check(&req)?;
        let name = req.into_inner().name;
        let deleted = self
            .state
            .services
            .meta
            .soft_delete_skill(&name)
            .await
            .map_err(|e| into_status(ApiError::from(e)))?;
        Ok(Response::new(app::DeleteSkillResponse { deleted }))
    }
}
