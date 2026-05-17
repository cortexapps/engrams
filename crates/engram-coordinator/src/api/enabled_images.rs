//! `/api/enabled-images` — operator-curated allow-list of OCI image
//! URIs that sessions may reference.
//!
//! Stage C: this is the single source of truth for "what images can a
//! session use." Postgres stores the URI plus a snapshot of the
//! manifest.toml fetched at enable time, so `POST /sessions` resolves
//! the manifest from a Postgres row without going to the registry on
//! the hot path. The dashboard's image picker reads from here.
//!
//! Verbs:
//! - `POST /api/enabled-images   { image_uri }` — enable an image:
//!   pull just the manifest layer from the registry (auth via
//!   `registry_credentials`), parse it as engram TOML to validate,
//!   upsert. Returns the new row.
//! - `GET  /api/enabled-images`                — list enabled rows.
//! - `POST /api/enabled-images/refresh { image_uri }` — re-fetch the
//!   manifest from the registry and update `manifest_toml` +
//!   `manifest_digest`. Useful when an image tag is moved.
//! - `POST /api/enabled-images/disable { image_uri }` — remove the
//!   row. Sessions referencing the image fail at the next create.
//!
//! URI as path-segment: image URIs contain `/` and `:`, which axum's
//! Path extractor will URL-decode but the dashboard's HTTP client
//! would have to encode. POSTing the URI in the body keeps both ends
//! simple and avoids the bikeshed.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use chrono::Utc;
use engram_core::types::session::split_image_ref;
use engram_core::types::snapshot::{SnapshotMetadata, SnapshotRecord};
use engram_core::types::template::TemplateRecord;
use engram_core::types::{EnabledImage, EnabledImageSummary, ImageManifest};
use engram_core::{MetaError, TemplateRef};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct EnableImageRequest {
    /// Full OCI URI: `<host>[/path]/<repo>:<tag>`. The host portion
    /// must match a row in `registry_credentials` for non-anonymous
    /// pulls; missing rows fall through to anonymous (works for
    /// public registries and `localhost:5001`).
    pub image_uri: String,
}

#[derive(Deserialize)]
pub struct ImageUriRequest {
    pub image_uri: String,
}

#[derive(Serialize)]
pub struct ListEnabledImagesResponse {
    pub images: Vec<EnabledImageSummary>,
}

pub async fn enable_image(
    State(state): State<SharedState>,
    Json(req): Json<EnableImageRequest>,
) -> Result<(StatusCode, Json<EnabledImageSummary>), ApiError> {
    if req.image_uri.trim().is_empty() {
        return Err(ApiError::BadRequest("image_uri must not be empty".into()));
    }

    let (row, manifest, bundle_canonical_snapshot) =
        fetch_and_seal_manifest(&state, &req.image_uri).await?;
    state
        .services
        .meta
        .upsert_enabled_image(row.clone())
        .await?;

    // ADR 0014 M1.11: if the OCI artifact's bundle.json carries a
    // canonical_snapshot block, cascade into snapshots + templates so
    // the host-agent's warm pool starts filling for this image on the
    // next heartbeat. Older bakes that pre-date M1.3 don't carry a
    // canonical_snapshot — we just enable the image (existing
    // behavior) and the session falls through to cold-create.
    if let Some(snapshot) = bundle_canonical_snapshot {
        if let Err(e) = cascade_into_templates(&state, &req.image_uri, &manifest, snapshot).await {
            // Don't fail the whole enable on cascade failure — the
            // image is still usable for cold-create. Surface the
            // error in logs so an operator can investigate.
            tracing::warn!(
                image_uri = %req.image_uri,
                error = %e,
                "enabled image; templates cascade failed (warm pool will not fire for this image)",
            );
        }
    }

    Ok((StatusCode::CREATED, Json(EnabledImageSummary::from(row))))
}

pub async fn list_enabled_images(
    State(state): State<SharedState>,
) -> Result<Json<ListEnabledImagesResponse>, ApiError> {
    let rows = state.services.meta.list_enabled_images().await?;
    let images = rows.into_iter().map(EnabledImageSummary::from).collect();
    Ok(Json(ListEnabledImagesResponse { images }))
}

pub async fn refresh_enabled_image(
    State(state): State<SharedState>,
    Json(req): Json<ImageUriRequest>,
) -> Result<Json<EnabledImageSummary>, ApiError> {
    if req.image_uri.trim().is_empty() {
        return Err(ApiError::BadRequest("image_uri must not be empty".into()));
    }

    // Refresh = "this URI must already be enabled, re-pull and update."
    // We require the row to exist so an operator can't accidentally
    // enable something via /refresh that they didn't enable via the
    // explicit POST path (which is the place that's auditable).
    let existing = state
        .services
        .meta
        .get_enabled_image(&req.image_uri)
        .await?
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "image `{}` is not enabled; call POST /api/enabled-images first",
                req.image_uri
            ))
        })?;

    let (mut refreshed, manifest, bundle_canonical_snapshot) =
        fetch_and_seal_manifest(&state, &req.image_uri).await?;
    // Preserve the original `id` + `created_at` so the row is
    // recognisably "the same row, refreshed" — `last_refreshed_at`
    // and `manifest_digest` are the ones that move.
    refreshed.id = existing.id;
    refreshed.created_at = existing.created_at;
    refreshed.updated_at = Some(Utc::now());

    state
        .services
        .meta
        .upsert_enabled_image(refreshed.clone())
        .await?;

    // ADR 0014 M1.11: refresh also re-runs the templates cascade, so
    // a registry image that was re-baked (same URI, new
    // canonical_snapshot) gets a fresh templates row pointing at the
    // new snapshot. upsert_template's internal transaction flips the
    // prior active row to false.
    if let Some(snapshot) = bundle_canonical_snapshot {
        if let Err(e) = cascade_into_templates(&state, &req.image_uri, &manifest, snapshot).await {
            tracing::warn!(
                image_uri = %req.image_uri,
                error = %e,
                "refreshed image; templates cascade failed (warm pool will not refill new snapshot)",
            );
        }
    }

    Ok(Json(EnabledImageSummary::from(refreshed)))
}

pub async fn disable_enabled_image(
    State(state): State<SharedState>,
    Json(req): Json<ImageUriRequest>,
) -> Result<StatusCode, ApiError> {
    match state
        .services
        .meta
        .delete_enabled_image(&req.image_uri)
        .await
    {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(MetaError::NotFound) => Err(ApiError::NotFound(format!(
            "image `{}` is not enabled",
            req.image_uri
        ))),
        Err(e) => Err(e.into()),
    }
}

/// Pull the manifest.toml + (optional) bundle.json layers of an
/// engram image artifact, validate them, and return:
///   - the `EnabledImage` row ready to upsert,
///   - the parsed `ImageManifest` (so the caller can read resource
///     hints for the templates cascade),
///   - the bundle's `canonical_snapshot` block if present (ADR
///     0014 M1.11 cascade input).
///
/// Bundle.json is optional: artifacts baked before ADR 0014 M1.3
/// don't carry it. We still enable the image; the cascade is skipped.
/// The caller preserves `id` + `created_at` on the EnabledImage if
/// this is a refresh; the new row here always carries fresh values.
async fn fetch_and_seal_manifest(
    state: &SharedState,
    image_uri: &str,
) -> Result<(EnabledImage, ImageManifest, Option<SnapshotMetadata>), ApiError> {
    let layers = state
        .services
        .oci
        .pull_engram_metadata(image_uri)
        .await
        .map_err(|e| {
            ApiError::BadRequest(format!(
                "registry pull for `{image_uri}` failed: {e}. \
                 Check that the URI is correct and that a matching \
                 registry credential exists if the registry requires auth."
            ))
        })?;

    let manifest_toml = String::from_utf8(layers.manifest_toml).map_err(|e| {
        ApiError::BadRequest(format!(
            "manifest layer for `{image_uri}` is not valid UTF-8: {e}"
        ))
    })?;

    // Validate the TOML parses cleanly. `manifest_toml` is the source
    // of truth; the parsed `ImageManifest` is consumed by the M1.11
    // cascade to read suggested_{vcpus,memory_mib}.
    let manifest: ImageManifest = toml::from_str(&manifest_toml).map_err(|e| {
        ApiError::BadRequest(format!(
            "manifest.toml at `{image_uri}` failed to parse as engram ImageManifest: {e}"
        ))
    })?;

    // Bundle.json parse is best-effort: if it's malformed or
    // canonical_snapshot is absent, the cascade simply doesn't fire.
    let canonical_snapshot = layers.bundle_json.as_deref().and_then(|bytes| {
        serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .and_then(|root| root.get("canonical_snapshot").cloned())
            .and_then(|snap| serde_json::from_value::<SnapshotMetadata>(snap).ok())
    });

    let now = Utc::now();
    let row = EnabledImage {
        id: Uuid::new_v4(),
        image_uri: image_uri.to_string(),
        manifest_toml,
        manifest_digest: layers.manifest_digest.as_str().to_string(),
        last_refreshed_at: now,
        created_at: now,
        updated_at: None,
    };
    Ok((row, manifest, canonical_snapshot))
}

/// ADR 0014 M1.11: insert `snapshots` + `templates` rows from a
/// just-pulled bundle's `canonical_snapshot` block. Uses two
/// separate writes — `record_snapshot` (upsert on id) followed by
/// `upsert_template` (which has its own internal transaction
/// flipping the prior active row). The two calls are not in one
/// outer transaction: `upsert_template`'s ON CONFLICT semantics
/// already cover idempotent re-enable, and a partial failure
/// (snapshots row inserted but templates upsert failed) leaves the
/// snapshots row inert — referenced by nothing, GC'd on the next
/// sweep. Acceptable for the M1.11 scope; M1.12+ may tighten this
/// if we observe orphans in prod.
async fn cascade_into_templates(
    state: &SharedState,
    image_uri: &str,
    manifest: &ImageManifest,
    snapshot: SnapshotMetadata,
) -> Result<(), ApiError> {
    let (image_repo, image_tag) = {
        let (repo, tag) = split_image_ref(image_uri);
        (repo.to_string(), tag.to_string())
    };
    if image_tag.is_empty() {
        return Err(ApiError::BadRequest(format!(
            "image `{image_uri}` has no tag suffix; templates cascade requires `<repo>:<tag>`"
        )));
    }

    // Defaults match the demo image's historical bake config; the
    // image's `resources` hints override when present. vcpus must be
    // >= 1 and memory_mib >= 128 (FC's documented minimum).
    let vcpus = manifest.resources.suggested_vcpus.unwrap_or(1).max(1);
    let memory_mib = manifest
        .resources
        .suggested_memory_mib
        .unwrap_or(512)
        .max(128);

    let now = Utc::now();
    let snapshot_record = SnapshotRecord {
        id: snapshot.id,
        // Template snapshots have no session_id (migration 0028).
        session_id: None,
        host_id: None,
        image_version: image_tag.clone(),
        size_bytes: snapshot.size_bytes,
        created_at: snapshot.created_at,
        last_accessed_at: now,
        disk_manifest: snapshot.disk_manifest,
        memory_manifest: snapshot.memory_manifest,
        // ADR 0014: canonical template snapshots are durable in
        // BlobStorage by construction. Mark recoverable so the M2
        // recovery path (when it lands) treats them as restorable.
        recoverable: true,
    };
    state.services.meta.record_snapshot(snapshot_record).await?;

    let template = TemplateRecord {
        template_ref: TemplateRef::new(),
        image_repo: image_repo.clone(),
        image_tag: image_tag.clone(),
        // ADR 0014 M1.11: pre-option-D, harness_pack_uri is still
        // part of the unique key. We write the sentinel "*" so the
        // template is shared across all harnesses the cold path may
        // request. M1.12's migration 0029 drops this column from
        // the unique key entirely.
        harness_pack_uri: "*".to_string(),
        snapshot_id: snapshot.id,
        vcpus,
        memory_mib,
        created_at: now,
        active: true,
    };
    state.services.meta.upsert_template(template).await?;

    tracing::info!(
        image_repo = %image_repo,
        image_tag = %image_tag,
        snapshot_id = %snapshot.id,
        vcpus,
        memory_mib,
        "templates cascade complete; warm pool will refill on next host heartbeat",
    );

    Ok(())
}
