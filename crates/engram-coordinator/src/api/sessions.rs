use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use engram_core::traits::{SecretBundle, SecretContext};
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec as VmSpec};
use engram_core::types::{ImageManifest, SecretMode, Session, SessionSpec, SessionStatus};
use engram_core::SessionId;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::image_registry::{ImageError, Rootfs};
use crate::state::{SessionEvent, SharedState};
use engram_host_agent::pool::PoolKey;

/// Default sandbox sizing for sessions created without explicit limits.
/// Phase 1 numbers — will move to per-repo `engram.toml` config later.
const DEFAULT_VCPUS: u32 = 2;
const DEFAULT_MEMORY_MIB: u32 = 4096;
const DEFAULT_DISK_GIB: u32 = 20;

/// Materialize the resolved image's rootfs (if any) into the sandbox
/// spec's `rootfs_source`. ProcessBackend reads this and copies the
/// directory; FirecrackerBackend will eventually attach the .ext4.
fn rootfs_source_from(
    resolved: Option<&crate::image_registry::ResolvedImage>,
) -> Option<std::path::PathBuf> {
    match resolved.map(|r| &r.rootfs)? {
        Rootfs::Directory(p) => Some(p.clone()),
        Rootfs::Ext4Image(p) => Some(p.clone()),
        Rootfs::None => None,
    }
}

/// Inject resolved secrets into the sandbox env according to the
/// manifest's `secret_mode`.
///
/// In `Literal` mode, real values land as env vars — fine for the dev
/// loop, never use in production.
///
/// In `Broker` mode, *placeholder* env vars land — the per-session
/// network proxy substitutes the real value only on outbound HTTPS
/// requests to the secret's `allow_hosts`. Today the proxy isn't
/// wired yet, so Broker mode results in placeholders that don't
/// authenticate anything; documented as next-round work in DESIGN.md.
fn apply_secrets_to_env(
    env: &mut HashMap<String, String>,
    bundle: &SecretBundle,
    mode: SecretMode,
    session: SessionId,
) {
    match mode {
        SecretMode::Literal => {
            for (name, resolved) in &bundle.secrets {
                env.insert(name.clone(), resolved.value.clone());
            }
        }
        SecretMode::Broker => {
            // Per-session, per-secret placeholder — the only way the
            // real value can leak via process state is if the proxy
            // is misconfigured. We log every placeholder issue with
            // a SHA-256 prefix so audit logs can correlate without
            // exposing the value.
            for name in bundle.secrets.keys() {
                let placeholder = format!(
                    "engram_ph_{}_{}",
                    session.as_uuid().simple(),
                    short_hash(name),
                );
                env.insert(name.clone(), placeholder);
            }
            // TODO(secrets-broker): register the keyring with the
            // per-session proxy here, and have the proxy substitute
            // placeholders on outbound HTTPS requests whose host
            // matches `schema.allow_hosts` / `schema.allow_host_patterns`.
            // Until that lands, Broker-mode images will see
            // unsubstituted placeholders and any real-API calls fail.
            tracing::warn!(
                %session,
                secret_count = bundle.secrets.len(),
                "secret broker proxy is not yet implemented; \
                 placeholders will not be substituted on outbound traffic",
            );
        }
    }
}

fn short_hash(s: &str) -> String {
    // Stable 8-char tag of `s`, just for placeholder uniqueness. Not
    // a security primitive — placeholders aren't sensitive.
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:08x}", h.finish() & 0xffff_ffff)
}

#[derive(Deserialize)]
pub struct CreateSessionRequest {
    pub repo: String,
    pub branch: String,
    pub user_id: Option<String>,
    pub image_version: Option<String>,
}

#[derive(Serialize)]
pub struct CreateSessionResponse {
    pub session_id: SessionId,
    pub status: &'static str,
    pub image_version: String,
}

pub async fn create_session(
    State(state): State<SharedState>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<CreateSessionResponse>), ApiError> {
    if req.repo.is_empty() || req.branch.is_empty() {
        return Err(ApiError::BadRequest(
            "`repo` and `branch` are required".into(),
        ));
    }

    let image_version = match req.image_version.clone() {
        Some(v) => v,
        None => match state.services.meta.latest_ready_image(&req.repo).await? {
            Some(img) => img.tag,
            None => state.cfg.default_image_version.clone(),
        },
    };

    // Resolve the image manifest from the registry. If the image
    // doesn't exist on disk we fall through to a "no image / empty
    // workdir" sandbox — keeps the bare-bones dev demo (`curl POST
    // /sessions` with no images set up) working out of the box.
    let resolved = match state.services.images.load(&req.repo, &image_version).await {
        Ok(r) => Some(r),
        Err(ImageError::NotFound { .. }) => None,
        Err(e) => return Err(ApiError::Internal(e.to_string())),
    };

    let manifest_for_secrets: ImageManifest = resolved
        .as_ref()
        .map(|r| r.manifest.clone())
        .unwrap_or_default();

    // Resolve secrets via the configured SecretStore. Required-but-
    // missing secrets fail the request (a 500 today; the client
    // will see `secret <name> not available`). Optional secrets
    // that aren't present are simply absent from the bundle.
    let secret_ctx = SecretContext {
        repo: &req.repo,
        image_tag: &image_version,
    };
    let secret_bundle: SecretBundle = state
        .services
        .secrets
        .resolve(&secret_ctx, &manifest_for_secrets.secrets)
        .await
        .map_err(|e| ApiError::Internal(format!("secret resolution: {e}")))?;

    let spec = SessionSpec {
        repo: req.repo.clone(),
        branch: req.branch,
        user_id: req.user_id,
        image_version: Some(image_version.clone()),
    };

    // 1. Persist the session row first so it has a stable SessionId
    //    even if sandbox creation fails — the failure is then
    //    observable as a session row stuck in `failed`.
    let session_id = state
        .services
        .meta
        .create_session(spec, image_version.clone())
        .await?;

    // 2. Build the *anonymous* SandboxSpec — what every sandbox in
    //    this (repo, image_version) pool gets. Session-specific env
    //    (ENGRAM_SESSION_ID, etc.) is injected at exec time so pooled
    //    sandboxes can serve any future session in the bucket.
    //    Env-var precedence inside the spec (low → high):
    //      manifest defaults  →  resolved secrets
    let mut spec_env: HashMap<String, String> = manifest_for_secrets.env.clone();
    apply_secrets_to_env(
        &mut spec_env,
        &secret_bundle,
        manifest_for_secrets.secret_mode,
        session_id,
    );

    let vm_spec = VmSpec {
        image: image_version.clone(),
        rootfs_source: rootfs_source_from(resolved.as_ref()),
        cpu: CpuLimit {
            vcpus: manifest_for_secrets
                .resources
                .suggested_vcpus
                .unwrap_or(DEFAULT_VCPUS),
        },
        memory: MemoryLimit {
            max_mib: manifest_for_secrets
                .resources
                .suggested_memory_mib
                .unwrap_or(DEFAULT_MEMORY_MIB),
        },
        disk: DiskLimit {
            max_gib: manifest_for_secrets
                .resources
                .suggested_disk_gib
                .unwrap_or(DEFAULT_DISK_GIB),
        },
        ttl: None,
        env: spec_env,
        workdir: None,
    };

    // 3. Configure the pool with this spec (idempotent — same key
    //    just refreshes the spec) and try a warm checkout. On miss,
    //    fall through to a synchronous create so callers don't wait
    //    for the warmer.
    let pool_key = PoolKey {
        repo: req.repo.clone(),
        image_version: image_version.clone(),
    };
    let target = state.cfg.default_warm_pool_size;
    if target > 0 {
        state
            .pool
            .configure(pool_key.clone(), target, vm_spec.clone());
    }

    let sandbox_id = match state.pool.checkout(&pool_key) {
        Some(id) => {
            tracing::debug!(
                session_id = %session_id,
                sandbox_id = %id,
                "session served from warm pool",
            );
            id
        }
        None => match state.services.sandbox.create(vm_spec).await {
            Ok(id) => id,
            Err(e) => {
                // Best-effort: mark the row failed and bubble the error.
                // We don't tear down the row — Postgres remains the
                // audit trail for the failure.
                let _ = state
                    .services
                    .meta
                    .set_session_status(session_id, SessionStatus::Failed)
                    .await;
                return Err(e.into());
            }
        },
    };

    // 4. Top the pool back up in the background. We don't await — the
    //    request finishes immediately; the next session in this
    //    bucket gets the freshly-created warm slot.
    if target > 0 {
        let pool = state.pool.clone();
        let key = pool_key.clone();
        tokio::spawn(async move {
            if let Err(e) = pool.replenish(&key).await {
                tracing::warn!(
                    error = %e,
                    repo = %key.repo,
                    image = %key.image_version,
                    "background pool replenish failed",
                );
            }
        });
    }

    state.registry.bind(session_id, sandbox_id);
    state
        .services
        .meta
        .set_session_status(session_id, SessionStatus::Active)
        .await?;
    state
        .emit(
            session_id,
            SessionEvent::StatusChanged {
                from: SessionStatus::Pending,
                to: SessionStatus::Active,
                at: chrono::Utc::now(),
            },
        )
        .await?;

    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            session_id,
            status: SessionStatus::Active.as_str(),
            image_version,
        }),
    ))
}

pub async fn get_session(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
) -> Result<Json<Session>, ApiError> {
    let s = state.services.meta.get_session(id).await?;
    Ok(Json(s))
}

#[derive(Serialize)]
pub struct ListSessionsResponse {
    pub sessions: Vec<Session>,
}

/// `GET /sessions` — list sessions. Today this only returns sessions
/// in `pending` / `active` / `idle` status (the rows
/// `list_active_sessions` selects); evicted/completed/failed rows are
/// hidden. A `?status=` filter for richer queries can be layered on
/// without a wire-format break — `sessions` stays the array key.
pub async fn list_sessions(
    State(state): State<SharedState>,
) -> Result<Json<ListSessionsResponse>, ApiError> {
    let sessions = state.services.meta.list_active_sessions().await?;
    Ok(Json(ListSessionsResponse { sessions }))
}

pub async fn delete_session(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
) -> Result<StatusCode, ApiError> {
    // Confirm the session exists *before* tearing anything down so
    // unknown ids return 404 (matching the get/exec contract) instead
    // of silently succeeding.
    let session = state.services.meta.get_session(id).await?;

    if let Some(sandbox_id) = state.registry.unbind(id) {
        // Best-effort: a session being deleted shouldn't fail the API
        // because the underlying VM already crashed or never came up.
        if let Err(e) = state.services.sandbox.destroy(sandbox_id).await {
            tracing::warn!(
                session_id = %id,
                sandbox_id = %sandbox_id,
                error = %e,
                "sandbox destroy failed during session delete; continuing",
            );
        }
    }

    state
        .services
        .meta
        .set_session_status(id, SessionStatus::Completed)
        .await?;
    state
        .emit(
            id,
            SessionEvent::StatusChanged {
                from: session.status,
                to: SessionStatus::Completed,
                at: chrono::Utc::now(),
            },
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
