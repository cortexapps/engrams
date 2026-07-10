//! `ImageService` over gRPC (ADR 0051 §2.3). Delegates to the same api
//! cores as the axum handlers; no admin gating — caller is trusted (ADR §6).

use std::sync::Arc;

use engram_protocol::app;
use tonic::{Request, Response, Status};

use super::{auth, convert, into_status};
use crate::state::SharedState;

pub struct AppImageService {
    pub state: SharedState,
    pub auth: Arc<auth::BearerAuth>,
}

/// Thin wrapper over [`crate::api::enabled_images::enqueue_enable_job`]
/// mapping its `ApiError` (incl. the in-flight-different-config
/// `Conflict`) onto a gRPC `Status`.
fn enqueue(
    state: &SharedState,
    image_uri: &str,
    config: &engram_core::types::image::ImageConfig,
    force_recapture: bool,
) -> impl std::future::Future<Output = Result<engram_core::types::EnableJob, Status>> {
    let state = state.clone();
    let image_uri = image_uri.to_string();
    let config = config.clone();
    async move {
        crate::api::enabled_images::enqueue_enable_job(&state, &image_uri, &config, force_recapture)
            .await
            .map_err(into_status)
    }
}

// EVERY RPC body starts with self.auth.check(&req)? — see auth.rs and the convention test.
#[tonic::async_trait]
impl app::image_service_server::ImageService for AppImageService {
    async fn list_enabled_images(
        &self,
        req: Request<app::ListEnabledImagesRequest>,
    ) -> Result<Response<app::ListEnabledImagesResponse>, Status> {
        self.auth.check(&req)?;
        let rows = self
            .state
            .services
            .meta
            .list_enabled_images()
            .await
            .map_err(|e| into_status(crate::error::ApiError::from(e)))?;
        let images = rows
            .into_iter()
            .map(|row| {
                let summary = engram_core::types::EnabledImageSummary::from(row);
                convert::enabled_image_summary_to_proto(&summary)
            })
            .collect();
        Ok(Response::new(app::ListEnabledImagesResponse { images }))
    }

    async fn enable_image(
        &self,
        req: Request<app::EnableImageRequest>,
    ) -> Result<Response<app::EnableImageResponse>, Status> {
        self.auth.check(&req)?;
        let req = req.into_inner();
        let image_uri = req.image_uri;
        if image_uri.trim().is_empty() {
            return Err(Status::invalid_argument("image_uri must not be empty"));
        }
        // ADR 0080: inherit-when-unset — an absent config keeps the
        // already-enabled row's config (so a plain re-enable / re-bake
        // roll doesn't wipe it); a set config replaces it wholesale. A
        // FIRST enable must supply it (there is nothing to inherit).
        let config = match &req.config {
            Some(c) => convert::image_config_from_proto(c).map_err(Status::invalid_argument)?,
            None => self
                .state
                .services
                .meta
                .get_enabled_image_any(&image_uri)
                .await
                .map_err(|e| into_status(crate::error::ApiError::from(e)))?
                .map(|r| r.image_config)
                .ok_or_else(|| {
                    Status::invalid_argument(format!(
                        "image `{image_uri}` is not enabled yet, so `config` is required \
                         on the first enable (name, [resources] suggested_vcpus, …)"
                    ))
                })?,
        };
        config
            .validate()
            .map_err(|e| Status::invalid_argument(format!("image config: {e}")))?;
        let job = enqueue(&self.state, &image_uri, &config, false).await?;
        Ok(Response::new(app::EnableImageResponse {
            job: Some(convert::enable_job_to_proto(&job)),
        }))
    }

    async fn update_image(
        &self,
        req: Request<app::UpdateImageRequest>,
    ) -> Result<Response<app::UpdateImageResponse>, Status> {
        self.auth.check(&req)?;
        let req = req.into_inner();
        let image_uri = req.image_uri;
        if image_uri.trim().is_empty() {
            return Err(Status::invalid_argument("image_uri must not be empty"));
        }
        let Some(config) = req.config.as_ref() else {
            return Err(Status::invalid_argument(
                "config is required (full replace)",
            ));
        };
        let config = convert::image_config_from_proto(config).map_err(Status::invalid_argument)?;
        config
            .validate()
            .map_err(|e| Status::invalid_argument(format!("image config: {e}")))?;
        // Live rows only — editing a disabled image is a re-enable's job.
        let existing = self
            .state
            .services
            .meta
            .get_enabled_image(&image_uri)
            .await
            .map_err(|e| into_status(crate::error::ApiError::from(e)))?
            .ok_or_else(|| {
                into_status(crate::error::ApiError::NotFound(format!(
                    "image `{image_uri}` is not enabled; enable it first"
                )))
            })?;

        // ADR 0080 mutability split: resources + anything under warm only
        // take effect through a base-snapshot recapture; everything else
        // (name/description/env/workdir) applies in place immediately.
        let mut recapture_fields: Vec<&str> = Vec::new();
        if existing.image_config.resources != config.resources {
            recapture_fields.push("resources");
        }
        if existing.image_config.warm != config.warm {
            recapture_fields.push("warm");
        }
        if recapture_fields.is_empty() {
            self.state
                .services
                .meta
                .update_enabled_image_config(&image_uri, &config)
                .await
                .map_err(|e| into_status(crate::error::ApiError::from(e)))?;
            return Ok(Response::new(app::UpdateImageResponse { job: None }));
        }
        if !req.allow_recapture {
            return Err(Status::failed_precondition(format!(
                "this edit changes {} — capture-affecting fields only take effect \
                 through a base-snapshot recapture (minutes; the enable pipeline \
                 re-runs). Re-send with allow_recapture = true to proceed.",
                recapture_fields.join(" and "),
            )));
        }
        let job = enqueue(&self.state, &image_uri, &config, false).await?;
        Ok(Response::new(app::UpdateImageResponse {
            job: Some(convert::enable_job_to_proto(&job)),
        }))
    }

    async fn disable_image(
        &self,
        req: Request<app::DisableImageRequest>,
    ) -> Result<Response<app::DisableImageResponse>, Status> {
        self.auth.check(&req)?;
        let image_uri = req.into_inner().image_uri;
        use engram_core::traits::DisableEnabledImageOutcome as Outcome;
        use engram_core::MetaError;
        let outcome = self
            .state
            .services
            .meta
            .soft_delete_enabled_image(&image_uri)
            .await
            .map_err(|e| match e {
                MetaError::NotFound => into_status(crate::error::ApiError::NotFound(format!(
                    "image `{image_uri}` is not enabled"
                ))),
                other => into_status(crate::error::ApiError::from(other)),
            })?;
        match outcome {
            Outcome::Disabled | Outcome::AlreadyDisabled => {
                Ok(Response::new(app::DisableImageResponse {}))
            }
            Outcome::Blocked(sessions) => {
                // image_in_use → FailedPrecondition (proto comment says so).
                // Include the first few blocking session ids so operators can
                // see exactly what is in the way without querying separately.
                const MAX_SHOWN: usize = 5;
                let shown: Vec<String> = sessions
                    .iter()
                    .take(MAX_SHOWN)
                    .map(|(sid, _)| sid.to_string())
                    .collect();
                let suffix = if sessions.len() > MAX_SHOWN {
                    format!(" … and {} more", sessions.len() - MAX_SHOWN)
                } else {
                    String::new()
                };
                Err(into_status(crate::error::ApiError::Conflict(format!(
                    "image `{image_uri}` is in use by {} active session(s): [{}]{}",
                    sessions.len(),
                    shown.join(", "),
                    suffix,
                ))))
            }
        }
    }

    async fn refresh_image(
        &self,
        req: Request<app::RefreshImageRequest>,
    ) -> Result<Response<app::RefreshImageResponse>, Status> {
        self.auth.check(&req)?;
        let req = req.into_inner();
        let image_uri = req.image_uri;
        if image_uri.trim().is_empty() {
            return Err(Status::invalid_argument("image_uri must not be empty"));
        }
        // Guard: URI must already be enabled.
        let existing = self
            .state
            .services
            .meta
            .get_enabled_image(&image_uri)
            .await
            .map_err(|e| into_status(crate::error::ApiError::from(e)))?;
        let Some(existing) = existing else {
            return Err(into_status(crate::error::ApiError::NotFound(format!(
                "image `{image_uri}` is not enabled; enable it first"
            ))));
        };
        // Refresh carries the existing config forward (no field to set on
        // the refresh request), so a re-pull of the same tag doesn't wipe it.
        let job = enqueue(
            &self.state,
            &image_uri,
            &existing.image_config,
            req.force_recapture,
        )
        .await?;
        Ok(Response::new(app::RefreshImageResponse {
            job: Some(convert::enable_job_to_proto(&job)),
        }))
    }

    async fn list_enable_jobs(
        &self,
        req: Request<app::ListEnableJobsRequest>,
    ) -> Result<Response<app::ListEnableJobsResponse>, Status> {
        self.auth.check(&req)?;
        let jobs = self
            .state
            .services
            .meta
            .list_enable_jobs(50)
            .await
            .map_err(|e| into_status(crate::error::ApiError::from(e)))?;
        let jobs = jobs
            .into_iter()
            .map(|j| convert::enable_job_to_proto(&j))
            .collect();
        Ok(Response::new(app::ListEnableJobsResponse { jobs }))
    }

    async fn get_enable_job(
        &self,
        req: Request<app::GetEnableJobRequest>,
    ) -> Result<Response<app::GetEnableJobResponse>, Status> {
        self.auth.check(&req)?;
        let job_id: uuid::Uuid = req
            .get_ref()
            .job_id
            .parse()
            .map_err(|_| Status::invalid_argument("malformed job_id"))?;
        let job = self
            .state
            .services
            .meta
            .get_enable_job(job_id)
            .await
            .map_err(|e| into_status(crate::error::ApiError::from(e)))?
            .ok_or_else(|| {
                into_status(crate::error::ApiError::NotFound(format!(
                    "enable job {job_id} not found"
                )))
            })?;
        Ok(Response::new(app::GetEnableJobResponse {
            job: Some(convert::enable_job_to_proto(&job)),
        }))
    }

    async fn retry_enable_job(
        &self,
        req: Request<app::RetryEnableJobRequest>,
    ) -> Result<Response<app::RetryEnableJobResponse>, Status> {
        self.auth.check(&req)?;
        let job_id: uuid::Uuid = req
            .get_ref()
            .job_id
            .parse()
            .map_err(|_| Status::invalid_argument("malformed job_id"))?;
        let job = self
            .state
            .services
            .meta
            .retry_enable_job(job_id)
            .await
            .map_err(|e| match e {
                engram_core::MetaError::NotFound => into_status(crate::error::ApiError::NotFound(
                    format!("enable job {job_id} not found"),
                )),
                engram_core::MetaError::Conflict(m) => {
                    into_status(crate::error::ApiError::Conflict(m))
                }
                other => into_status(crate::error::ApiError::from(other)),
            })?;
        Ok(Response::new(app::RetryEnableJobResponse {
            job: Some(convert::enable_job_to_proto(&job)),
        }))
    }

    async fn list_registries(
        &self,
        req: Request<app::ListRegistriesRequest>,
    ) -> Result<Response<app::ListRegistriesResponse>, Status> {
        self.auth.check(&req)?;
        let creds = self
            .state
            .services
            .meta
            .list_registry_credentials()
            .await
            .map_err(|e| into_status(crate::error::ApiError::from(e)))?;
        let registries = creds
            .into_iter()
            .map(|c| {
                let s = engram_core::types::RegistryCredentialSummary::from(c);
                convert::registry_credential_summary_to_proto(&s)
            })
            .collect();
        Ok(Response::new(app::ListRegistriesResponse { registries }))
    }

    async fn add_registry(
        &self,
        req: Request<app::AddRegistryRequest>,
    ) -> Result<Response<app::AddRegistryResponse>, Status> {
        self.auth.check(&req)?;
        let r = req.into_inner();
        let api_req = convert::add_registry_request_from_proto(r).map_err(into_status)?;
        let resp = crate::api::registries::add_registry_core(&self.state, api_req)
            .await
            .map_err(into_status)?;
        Ok(Response::new(app::AddRegistryResponse {
            id: resp.id.to_string(),
            host: resp.host,
            auth_kind: resp.auth_kind,
            auth_principal: resp.auth_principal,
        }))
    }

    async fn delete_registry(
        &self,
        req: Request<app::DeleteRegistryRequest>,
    ) -> Result<Response<app::DeleteRegistryResponse>, Status> {
        self.auth.check(&req)?;
        let host = req.into_inner().host;
        use engram_core::MetaError;
        match self
            .state
            .services
            .meta
            .delete_registry_credential(&host)
            .await
        {
            Ok(()) => Ok(Response::new(app::DeleteRegistryResponse {})),
            Err(MetaError::NotFound) => Err(into_status(crate::error::ApiError::NotFound(
                format!("no registry credential for host {host:?}"),
            ))),
            Err(e) => Err(into_status(crate::error::ApiError::from(e))),
        }
    }
}
