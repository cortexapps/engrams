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
use crate::host_registry::ScheduleContext;
use crate::image_registry::{ImageError, Rootfs};
use crate::state::{SessionEvent, SharedState};

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
    /// Phase 4: clones the repo at create but never pushes back; no
    /// checkpoint branch allocated. Defaults to false (writable git
    /// session for `git+...` repos; ephemeral for `local://`).
    #[serde(default)]
    pub read_only: bool,
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
        read_only: req.read_only,
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

    let agent = build_dev_noop_agent(&state, session_id);

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
        agent,
    };

    // 3. Scheduler picks a host based on heartbeat-derived state
    //    (snapshot affinity → warm-pool match → capacity), then the
    //    host's `PooledBackend` opportunistically returns a warm slot
    //    or creates fresh as needed. The pool, replenish loop, and
    //    spec storage all live host-side now — the coordinator just
    //    routes.
    let ctx = ScheduleContext {
        repo: &req.repo,
        image_version: &image_version,
        prefer_snapshot_id: None,
        memory_mib: Some(vm_spec.memory.max_mib),
    };
    let sandbox_id = match state.host_registry.create_for_session(&ctx, vm_spec).await {
        Ok((host_id, id)) => {
            if let Err(e) = state
                .services
                .meta
                .assign_session_host(session_id, Some(host_id))
                .await
            {
                tracing::warn!(
                    session_id = %session_id,
                    host_id = %host_id,
                    error = %e,
                    "assign_session_host failed; routing still works via HostRegistry"
                );
            }
            // Persist the sandbox_id so a coordinator restart can
            // rebuild its in-memory routing maps from `sessions`.
            if let Err(e) = state
                .services
                .meta
                .assign_session_sandbox(session_id, Some(id))
                .await
            {
                tracing::warn!(
                    session_id = %session_id,
                    sandbox_id = %id,
                    error = %e,
                    "assign_session_sandbox failed; live routing still works (in-memory only)"
                );
            }
            id
        }
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
    };

    state.registry.bind(session_id, sandbox_id);
    // Bind the routing in the hub *before* the agent has a chance
    // to dial. `backend.create()` returns with the sandbox ready
    // to accept exec but the agent (if any) NOT yet spawned —
    // the trait splits create from `start_agent` precisely so
    // this window can be filled with whatever routing setup the
    // caller needs.
    state.harness_hub.bind_session(session_id, sandbox_id);
    // Now release the agent into the world. If the spec carried
    // no agent (production sessions today), this is a no-op.
    if let Err(e) = state.services.sandbox.start_agent(sandbox_id).await {
        // Agent failed to spawn: surface as Internal but leave the
        // session row so the caller sees a clear error chain.
        let _ = state
            .services
            .meta
            .set_session_status(session_id, SessionStatus::Failed)
            .await;
        state.harness_hub.unbind_session(session_id);
        return Err(e.into());
    }
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

/// `POST /sessions/:id/migrate { host_id? }`. Operator-initiated
/// migration. Clears the session's `host_id`, transitions it to
/// `PendingReassign`, and tears down its live sandbox if any.
/// The actual restore happens on the next call to `/resume` (or any
/// access that ends up rescheduling — Phase 3d kept this lazy to
/// keep the coordinator off the data path).
///
/// Pre-condition: a snapshot must exist for the session — otherwise
/// the next `/resume` will fail. Migrate refuses to transition a
/// session that has no snapshot so the operator notices early.
#[derive(Deserialize)]
pub struct MigrateRequest {
    #[serde(default)]
    pub host_id: Option<engram_core::HostId>,
}

#[derive(Serialize)]
pub struct MigrateResponse {
    pub session_id: SessionId,
    pub status: &'static str,
    pub note: &'static str,
}

pub async fn migrate(
    State(state): State<SharedState>,
    Path(id): Path<SessionId>,
    Json(req): Json<MigrateRequest>,
) -> Result<Json<MigrateResponse>, ApiError> {
    let session = state.services.meta.get_session(id).await?;
    if matches!(
        session.status,
        SessionStatus::Completed | SessionStatus::Failed
    ) {
        return Err(ApiError::Conflict(format!(
            "session is {} — terminal sessions can't migrate",
            session.status.as_str()
        )));
    }

    if state
        .services
        .meta
        .latest_snapshot_for_session(id)
        .await?
        .is_none()
    {
        return Err(ApiError::Conflict(
            "no snapshot exists for this session — take a snapshot before migrating".into(),
        ));
    }

    // Optional target host validation. The actual host is picked by
    // the scheduler on next access; we just sanity-check that the
    // hint refers to a ready host so a typo errors immediately.
    if let Some(target) = req.host_id {
        let hosts = state.services.meta.list_active_hosts().await?;
        let target_row = hosts.iter().find(|h| h.id == target);
        match target_row {
            Some(h) if h.status == engram_core::types::HostStatus::Ready => {}
            Some(h) => {
                return Err(ApiError::BadRequest(format!(
                    "target host {target} is in {} state, not ready",
                    h.status.as_str()
                )));
            }
            None => {
                return Err(ApiError::NotFound(format!("host {target} not registered")));
            }
        }
    }

    // Tear down the live sandbox if any. Best-effort — the migration
    // proceeds even if destroy errors.
    if let Some(sandbox_id) = state.registry.unbind(id) {
        if let Err(e) = state.services.sandbox.destroy(sandbox_id).await {
            tracing::warn!(
                session_id = %id,
                sandbox_id = %sandbox_id,
                error = %e,
                "destroy during migrate failed; continuing",
            );
        }
    }

    let from_host = session.host_id;
    state.services.meta.assign_session_host(id, None).await?;
    // Clear sandbox_id alongside host_id — the sandbox was just
    // destroyed, and the next /resume will land on a new host with
    // a fresh sandbox_id.
    let _ = state.services.meta.assign_session_sandbox(id, None).await;
    state
        .services
        .meta
        .set_session_status(id, SessionStatus::PendingReassign)
        .await?;
    let now = chrono::Utc::now();
    state
        .emit(
            id,
            SessionEvent::StatusChanged {
                from: session.status,
                to: SessionStatus::PendingReassign,
                at: now,
            },
        )
        .await?;
    if let Some(from_host) = from_host {
        tracing::info!(
            session_id = %id,
            from_host = %from_host,
            "session migrated; awaiting reassignment on next /resume"
        );
    }

    Ok(Json(MigrateResponse {
        session_id: id,
        status: SessionStatus::PendingReassign.as_str(),
        note: "session marked for reassignment; call /resume to land it on a new host",
    }))
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

    // Clear sandbox_id since the sandbox is destroyed; the session
    // row is now terminal and won't be repopulated by startup
    // routing rebuild even if it kept the column set, but tidy
    // anyway so an audit query "what sandboxes does the coordinator
    // think exist" matches reality.
    let _ = state.services.meta.assign_session_sandbox(id, None).await;
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
    state.harness_hub.unbind_session(id);
    Ok(StatusCode::NO_CONTENT)
}

/// Build the `AgentSpec` for the dev `engram-harness-noop` binary if
/// `cfg.dev_auto_noop` is set, the harness binary path is configured,
/// and the harness TCP listener is bound. None otherwise — production
/// callers don't auto-spawn anything; their harness lives inside the
/// rootfs and is invoked by `engram-bootstrap`.
fn build_dev_noop_agent(
    state: &SharedState,
    session_id: SessionId,
) -> Option<engram_core::types::sandbox::AgentSpec> {
    if !state.cfg.dev_auto_noop {
        return None;
    }
    let bin = state.cfg.dev_noop_harness_path.as_ref()?.to_string_lossy().to_string();
    let addr = (*state.harness_listen_addr.lock())?;
    let mut env = HashMap::new();
    env.insert("ENGRAM_HARNESS_ADDR".into(), addr.to_string());
    env.insert("ENGRAM_SESSION_ID".into(), session_id.to_string());
    Some(engram_core::types::sandbox::AgentSpec {
        argv: vec![
            bin,
            "--connect".into(),
            addr.to_string(),
            "--session-id".into(),
            session_id.to_string(),
        ],
        env,
    })
}
