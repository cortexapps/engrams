//! `/api/harnesses` — list / register / delete harness packs.
//!
//! Stage B2: harness packs live in a Docker registry and are indexed
//! by Postgres `harness_packs` rows. This module owns the CRUD
//! surface; the host-agent pulls the pack at session-create time and
//! assembles a per-session ext4 substrate that gets mounted into the
//! sandbox at `/run/engram/harnesses`.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use engram_core::types::registry::HarnessPack;
use engram_core::MetaError;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::SharedState;

#[derive(Serialize)]
pub struct HarnessDescriptor {
    pub name: String,
    pub description: Option<String>,
    pub registry_uri: String,
}

#[derive(Deserialize)]
pub struct AddHarnessRequest {
    pub name: String,
    pub registry_uri: String,
    pub description: Option<String>,
}

pub async fn list_harnesses(
    State(state): State<SharedState>,
) -> Result<Json<Vec<HarnessDescriptor>>, ApiError> {
    let packs = state.services.meta.list_harness_packs().await?;
    let descriptors = packs
        .into_iter()
        .map(|p| HarnessDescriptor {
            name: p.name,
            description: p.description,
            registry_uri: p.registry_uri,
        })
        .collect();
    Ok(Json(descriptors))
}

pub async fn add_harness(
    State(state): State<SharedState>,
    Json(req): Json<AddHarnessRequest>,
) -> Result<(StatusCode, Json<HarnessDescriptor>), ApiError> {
    if req.name.trim().is_empty() {
        return Err(ApiError::BadRequest("name must not be empty".into()));
    }
    if req.registry_uri.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "registry_uri must not be empty".into(),
        ));
    }

    let pack = HarnessPack {
        id: Uuid::new_v4(),
        name: req.name.clone(),
        registry_uri: req.registry_uri.clone(),
        description: req.description.clone(),
        created_at: Utc::now(),
        updated_at: None,
    };
    state
        .services
        .meta
        .upsert_harness_pack(pack.clone())
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(HarnessDescriptor {
            name: pack.name,
            description: pack.description,
            registry_uri: pack.registry_uri,
        }),
    ))
}

pub async fn delete_harness(
    State(state): State<SharedState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    match state.services.meta.delete_harness_pack(&name).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(MetaError::NotFound) => Err(ApiError::NotFound(format!(
            "no harness pack registered with name {name:?}"
        ))),
        Err(e) => Err(e.into()),
    }
}
