//! `GET /api/images` — list every image the registry knows about,
//! projected for the dashboard's "create session" form.
//!
//! The dashboard renders an image dropdown plus, *per image*, one
//! masked input per declared `[secrets.X]` entry. Returning the
//! manifest's secret schema here is what lets the form be image-driven
//! (no hard-coded "API key vs OAuth" branching in the UI).
//!
//! Harnesses do NOT live on images — they're a deployment-wide host
//! concern, served by [`super::harnesses::list_harnesses`].

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::error::ApiError;
use crate::state::SharedState;

#[derive(Serialize)]
pub struct ImageDescriptor {
    pub repo: String,
    pub tag: String,
    pub name: String,
    pub description: Option<String>,
    /// `"literal"` or `"broker"`. The dashboard rejects browser-pasted
    /// secrets for `broker` images (the coordinator does too — this
    /// is just so the UI can disable the inputs up front).
    pub secret_mode: String,
    pub required_secrets: Vec<RequiredSecret>,
}

#[derive(Serialize)]
pub struct RequiredSecret {
    pub name: String,
    pub required: bool,
    pub allow_hosts: Vec<String>,
}

pub async fn list_images(
    State(state): State<SharedState>,
) -> Result<Json<Vec<ImageDescriptor>>, ApiError> {
    let images = state
        .services
        .images
        .list()
        .await
        .map_err(|e| ApiError::Internal(format!("image registry list: {e}")))?;

    let descriptors = images
        .into_iter()
        .map(|img| {
            let secret_mode = match img.manifest.secret_mode {
                engram_core::types::image::SecretMode::Literal => "literal",
                engram_core::types::image::SecretMode::Broker => "broker",
            };
            let required_secrets = img
                .manifest
                .secrets
                .iter()
                .map(|(name, schema)| RequiredSecret {
                    name: name.clone(),
                    required: schema.required,
                    allow_hosts: schema.allow_hosts.clone(),
                })
                .collect();
            ImageDescriptor {
                repo: img.repo,
                tag: img.tag,
                name: img.manifest.name,
                description: img.manifest.description,
                secret_mode: secret_mode.to_string(),
                required_secrets,
            }
        })
        .collect();

    Ok(Json(descriptors))
}
