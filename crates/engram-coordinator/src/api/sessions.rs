use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use engram_core::traits::{SecretBundle, SecretContext};
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec as VmSpec};
use engram_core::types::session::{HarnessSpec, ImageRef, WorkspaceSpec};
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
    /// Which baked image to boot. Required.
    pub image: ImageRef,
    /// Where the workspace comes from. Required.
    pub workspace: WorkspaceSpec,
    /// What agent process (if any) to attach. Defaults to
    /// `HarnessSpec::None` — boot the VM and let the user drive it
    /// via the in-browser shell or `engram exec`.
    #[serde(default)]
    pub harness: HarnessSpec,
    pub user_id: Option<String>,
    /// Initial prompt for the agent. Only meaningful when
    /// `harness != None`; the API rejects with 400 when set
    /// alongside `harness = None` (silent drop is a footgun).
    #[serde(default)]
    pub prompt: Option<String>,
    /// Per-request secret values keyed by env-var name. Only
    /// honored under `SecretMode::Literal`; broker-mode images
    /// reject overrides.
    #[serde(default)]
    pub secrets: Option<HashMap<String, String>>,
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
    use crate::config::SandboxBackendChoice;

    // -------- 0. Validate orthogonal axes --------
    // `harness=None + non-empty prompt` is meaningless: there's no
    // agent to consume the prompt. Silent drop hides the bug; reject
    // explicitly so the dashboard / CLI surfaces the mistake.
    if matches!(req.harness, HarnessSpec::None)
        && req.prompt.as_deref().map(str::is_empty).unwrap_or(true) == false
    {
        return Err(ApiError::BadRequest(
            "`prompt` requires a `harness` other than `none` — there is no agent to receive it"
                .into(),
        ));
    }
    // `LocalMount` requires host→guest sharing. FC's virtio-fs
    // story is future work; reject upfront when the active backend
    // doesn't advertise support rather than silently dropping the
    // mount inside the backend. Track FC parity in the plan's
    // "open questions" §3.
    if matches!(req.workspace, WorkspaceSpec::LocalMount { .. })
        && !state.services.sandbox.supports_local_mount()
    {
        return Err(ApiError::BadRequest(
            "`workspace.local_mount` is not supported by the active sandbox backend (typically Firecracker); use VZ or Process"
                .into(),
        ));
    }

    let ImageRef::Registry { repo: image_repo, tag: image_tag } = req.image.clone();

    // -------- 1. Resolve image manifest --------
    // Image is required and must exist in the registry. The fallback
    // "no image / empty workdir" path that used to support the bare
    // `curl POST /sessions` demo is gone — every session declares an
    // explicit image, full stop.
    let resolved = match state.services.images.load(&image_repo, &image_tag).await {
        Ok(r) => r,
        Err(ImageError::NotFound { .. }) => {
            return Err(ApiError::BadRequest(format!(
                "image `{image_repo}:{image_tag}` not found in registry"
            )));
        }
        Err(e) => return Err(ApiError::Internal(e.to_string())),
    };
    let manifest = resolved.manifest.clone();

    // Validate the harness against the image manifest. Builtin names
    // are scoped per-image — a session asking for `claude` on an
    // image that doesn't bake `claude` is a 400, not a runtime
    // surprise.
    if let HarnessSpec::Builtin { name } = &req.harness {
        if !manifest.harnesses.iter().any(|h| h.name == *name) {
            let available: Vec<&str> = manifest.harnesses.iter().map(|h| h.name.as_str()).collect();
            return Err(ApiError::BadRequest(format!(
                "image `{image_repo}:{image_tag}` does not bake harness `{name}` \
                 (available: {available:?})"
            )));
        }
    }

    // -------- 2. Resolve secrets --------
    let secret_ctx = SecretContext {
        repo: &image_repo,
        image_tag: &image_tag,
    };
    if req.secrets.as_ref().map(|m| !m.is_empty()).unwrap_or(false)
        && manifest.secret_mode != engram_core::types::image::SecretMode::Literal
    {
        return Err(ApiError::BadRequest(
            "per-request `secrets` are only supported for `secret_mode = literal` images".into(),
        ));
    }
    let secret_bundle: SecretBundle = state
        .services
        .secrets
        .resolve(&secret_ctx, &manifest.secrets, req.secrets.as_ref())
        .await
        .map_err(|e| ApiError::Internal(format!("secret resolution: {e}")))?;

    let spec = SessionSpec {
        image: req.image.clone(),
        workspace: req.workspace.clone(),
        harness: req.harness.clone(),
        user_id: req.user_id,
    };

    // -------- 3. Persist the row --------
    // First so it has a stable SessionId even if sandbox creation
    // fails — the failure is then observable as a row stuck in
    // `failed`.
    let session_id = state.services.meta.create_session(spec).await?;

    // -------- 4. Build the anonymous SandboxSpec --------
    // What every sandbox in this image pool gets. Session-specific
    // env (ENGRAM_SESSION_ID etc.) is injected at exec / start_agent
    // time so pooled sandboxes can serve any future session in the
    // bucket.
    let mut spec_env: HashMap<String, String> = manifest.env.clone();
    apply_secrets_to_env(&mut spec_env, &secret_bundle, manifest.secret_mode, session_id);

    let agent_for_session = resolve_harness(
        &state,
        &manifest,
        &resolved,
        &req.harness,
        session_id,
        req.prompt.as_deref(),
        &spec_env,
    )?;

    // LocalMount workspaces wire host paths into the spec at create
    // time so the backend's virtio-fs / bind layer sees them before
    // the VM boots; Empty / Git workspaces don't need mounts.
    let mounts: Vec<engram_core::types::sandbox::MountSpec> = match &req.workspace {
        WorkspaceSpec::LocalMount {
            host_path,
            guest_path,
            read_only,
        } => vec![engram_core::types::sandbox::MountSpec {
            host_path: host_path.clone(),
            guest_path: guest_path.clone(),
            read_only: *read_only,
        }],
        _ => Vec::new(),
    };

    let vm_spec = VmSpec {
        image: image_tag.clone(),
        rootfs_source: rootfs_source_from(Some(&resolved)),
        cpu: CpuLimit {
            vcpus: manifest.resources.suggested_vcpus.unwrap_or(DEFAULT_VCPUS),
        },
        memory: MemoryLimit {
            max_mib: manifest
                .resources
                .suggested_memory_mib
                .unwrap_or(DEFAULT_MEMORY_MIB),
        },
        disk: DiskLimit {
            max_gib: manifest
                .resources
                .suggested_disk_gib
                .unwrap_or(DEFAULT_DISK_GIB),
        },
        ttl: None,
        env: spec_env,
        workdir: None,
        mounts,
    };

    // -------- 5. Schedule + create the sandbox --------
    let ctx = ScheduleContext {
        repo: &image_repo,
        image_version: &image_tag,
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
    // to accept exec but the agent (if any) NOT yet spawned.
    state.harness_hub.bind_session(session_id, sandbox_id);

    // -------- 6. Materialize workspace --------
    // Runs while the row is still `Pending`. A failure here marks
    // the row Failed before the user ever sees Active — a clear
    // "git clone broke" beats a green session with no checkout.
    if let Err(e) = crate::workspace::materialize(
        &*state.services.sandbox,
        sandbox_id,
        &req.workspace,
    )
    .await
    {
        tracing::warn!(
            session_id = %session_id,
            error = %e,
            "workspace materialization failed",
        );
        let _ = state
            .services
            .meta
            .set_session_status(session_id, SessionStatus::Failed)
            .await;
        state.harness_hub.unbind_session(session_id);
        return Err(ApiError::Internal(format!("workspace materialization: {e}")));
    }

    // -------- 7. Start the agent (if any) --------
    if let Some(agent) = agent_for_session {
        if let Err(e) = state.services.sandbox.start_agent(sandbox_id, agent).await {
            let _ = state
                .services
                .meta
                .set_session_status(session_id, SessionStatus::Failed)
                .await;
            state.harness_hub.unbind_session(session_id);
            return Err(e.into());
        }
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

    // If the request carried an initial prompt, record it as a
    // user-role message in the event log. The harness pulls the value
    // out of `ENGRAM_INITIAL_PROMPT` and runs it without echoing it
    // back through Claude's stream-json output, so subscribers
    // (transcripts, dashboards) only see the assistant's reply
    // otherwise. Mirror the per-prompt path in `prompt.rs`. Best-
    // effort: a failed emit doesn't roll back session creation.
    if let Some(text) = req.prompt.as_deref().filter(|s| !s.is_empty()) {
        if let Err(e) = state
            .emit(
                session_id,
                SessionEvent::HarnessAgentMessage {
                    run_id: String::new(),
                    message_id: format!("user-{}", uuid::Uuid::new_v4()),
                    role: engram_harness_proto::AgentRole::User,
                    text: text.to_string(),
                    at: chrono::Utc::now(),
                },
            )
            .await
        {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "emit initial prompt event failed",
            );
        }
    }

    Ok((
        StatusCode::CREATED,
        Json(CreateSessionResponse {
            session_id,
            status: SessionStatus::Active.as_str(),
            image_version: image_tag,
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

/// Phase-0 transition shim. Translate the legacy `repo` URL scheme
/// Resolve a session's [`HarnessSpec`] against the image manifest +
/// the resolved image dir, building the [`AgentSpec`] the backend
/// will spawn at `start_agent` time. `None` is returned for
/// `HarnessSpec::None` — the sandbox boots with no agent, leaving
/// the user to drive it via the in-browser shell or `engram exec`.
///
/// Builtin harnesses live at `manifest.harnesses[].guest_path`
/// inside the rootfs. Two argv shapes depending on backend:
/// - `Process`: `--connect host:port`. The binary is exec'd as a
///   host subprocess so `argv[0]` is the *host* path (the image
///   registry's exported `<rootfs_dir>` + the manifest's
///   `guest_path`).
/// - `Firecracker` / `VZ`: `--vsock-host <port>`. The in-VM bootstrap
///   exec's the binary inside the rootfs so `argv[0]` is the
///   manifest's `guest_path` directly.
pub(crate) fn resolve_harness(
    state: &SharedState,
    manifest: &ImageManifest,
    resolved_image: &crate::image_registry::ResolvedImage,
    spec: &HarnessSpec,
    session_id: SessionId,
    initial_prompt: Option<&str>,
    base_env: &HashMap<String, String>,
) -> Result<Option<engram_core::types::sandbox::AgentSpec>, ApiError> {
    use crate::config::SandboxBackendChoice;
    let name = match spec {
        HarnessSpec::None => return Ok(None),
        HarnessSpec::Builtin { name } => name,
    };
    let entry = manifest
        .harnesses
        .iter()
        .find(|h| h.name == *name)
        .ok_or_else(|| {
            ApiError::Internal(format!(
                "harness `{name}` validated as available but vanished from manifest"
            ))
        })?;

    let mut env: HashMap<String, String> = base_env.clone();
    env.insert("ENGRAM_SESSION_ID".into(), session_id.to_string());
    if let Some(prompt) = initial_prompt {
        env.insert("ENGRAM_INITIAL_PROMPT".into(), prompt.to_string());
    }

    let argv = match state.services.sandbox.harness_dial() {
        engram_core::traits::HarnessDial::HostTcp => {
            let addr = match *state.harness_listen_addr.lock() {
                Some(addr) => addr,
                None => {
                    return Err(ApiError::Internal(
                        "harness listener not bound (HostTcp backend)".into(),
                    ));
                }
            };
            env.insert("ENGRAM_HARNESS_ADDR".into(), addr.to_string());
            let host_path = host_tcp_harness_host_path(resolved_image, &entry.guest_path)?;
            vec![
                host_path,
                "--connect".into(),
                addr.to_string(),
                "--session-id".into(),
                session_id.to_string(),
            ]
        }
        engram_core::traits::HarnessDial::Vsock => {
            let port = engram_harness_proto::HARNESS_VSOCK_PORT;
            vec![
                entry.guest_path.clone(),
                "--vsock-host".into(),
                port.to_string(),
                "--session-id".into(),
                session_id.to_string(),
            ]
        }
    };
    Ok(Some(engram_core::types::sandbox::AgentSpec { argv, env }))
}

/// Resolve `guest_path` (e.g. `/sbin/engram-harness-claude`) against
/// the image's exported rootfs directory so a `HostTcp`-dialing
/// backend can exec it as a host subprocess. The image registry
/// stores the rootfs at `<images_dir>/<repo>/<tag>/rootfs/`; strip
/// the leading `/` from `guest_path` and join.
fn host_tcp_harness_host_path(
    resolved: &crate::image_registry::ResolvedImage,
    guest_path: &str,
) -> Result<String, ApiError> {
    use crate::image_registry::Rootfs;
    let rel = guest_path.trim_start_matches('/');
    if rel.is_empty() {
        return Err(ApiError::BadRequest("harness `guest_path` is empty".into()));
    }
    match &resolved.rootfs {
        Rootfs::Directory(dir) => Ok(dir.join(rel).to_string_lossy().into_owned()),
        Rootfs::Ext4Image(_) => Err(ApiError::Internal(
            "HostTcp-dialing backend cannot exec a harness baked into an ext4 rootfs; \
             rebuild the image with `--format directory`"
                .into(),
        )),
        Rootfs::None => Err(ApiError::BadRequest(
            "image has no rootfs — `harness.builtin` requires a baked binary".into(),
        )),
    }
}
