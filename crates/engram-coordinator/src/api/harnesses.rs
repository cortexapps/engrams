//! `GET /api/harnesses` — list every harness the host has registered.
//!
//! Harnesses live above images: the host owns a directory
//! (`cfg.harnesses_dir`) of binaries that are mounted read-only into
//! every sandbox at `/run/engram/harnesses` via virtio-fs (or
//! symlink, on Process). The dashboard's per-session "harness"
//! dropdown is populated from this endpoint — same set across all
//! images.

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::error::ApiError;
use crate::state::SharedState;

#[derive(Serialize)]
pub struct HarnessDescriptor {
    pub name: String,
    pub description: Option<String>,
}

pub async fn list_harnesses(
    State(state): State<SharedState>,
) -> Result<Json<Vec<HarnessDescriptor>>, ApiError> {
    let descriptors = state
        .services
        .harnesses
        .entries()
        .iter()
        .map(|h| HarnessDescriptor {
            name: h.name.clone(),
            description: h.description.clone(),
        })
        .collect();
    Ok(Json(descriptors))
}
