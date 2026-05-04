use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use engram_core::traits::{SecretBundle, SecretContext};
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec as VmSpec};
use engram_core::types::session::{split_image_ref, HarnessSpec, ImageRef, WorkspaceSpec};
use engram_core::types::{ImageManifest, SecretMode, Session, SessionSpec, SessionStatus};
use engram_core::SessionId;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::host_registry::ScheduleContext;
use crate::state::{SessionEvent, SharedState};

/// Default sandbox sizing for sessions created without explicit limits.
/// Phase 1 numbers — will move to per-repo `engram.toml` config later.
const DEFAULT_VCPUS: u32 = 2;
const DEFAULT_MEMORY_MIB: u32 = 4096;
const DEFAULT_DISK_GIB: u32 = 20;

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

/// Extract the hostname from a clone URL so it can be added to the
/// session's `allow_hosts`. Tolerates `git+https://`, `git@host:path`
/// (SSH), and plain http(s). Returns `None` for shapes the FC net
/// layer can't allowlist by name (e.g. `git://`, `file://`).
fn host_from_url(raw: &str) -> Option<String> {
    if let Ok(u) = url::Url::parse(raw) {
        return u.host_str().map(str::to_owned);
    }
    // SSH form: `git@github.com:cortex/api.git`. Take the substring
    // between `@` and the first `:`.
    if let Some(rest) = raw
        .strip_prefix("git@")
        .or_else(|| raw.split_once('@').map(|(_, r)| r))
    {
        if let Some((host, _)) = rest.split_once(':') {
            if !host.is_empty() {
                return Some(host.to_string());
            }
        }
    }
    None
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
    // -------- 0. Validate orthogonal axes --------
    // `harness=None + non-empty prompt` is meaningless: there's no
    // agent to consume the prompt. Silent drop hides the bug; reject
    // explicitly so the dashboard / CLI surfaces the mistake.
    if matches!(req.harness, HarnessSpec::None)
        && !req.prompt.as_deref().map(str::is_empty).unwrap_or(true)
    {
        return Err(ApiError::BadRequest(
            "`prompt` requires a `harness` other than `none` — there is no agent to receive it"
                .into(),
        ));
    }

    let image_uri: ImageRef = req.image.clone();
    let (image_repo, image_tag) = {
        let (r, t) = split_image_ref(&image_uri);
        (r.to_string(), t.to_string())
    };

    // -------- 1. Look up the manifest from `enabled_images` --------
    // The session's image must be enabled before sessions can
    // reference it. The `enabled_images` row carries the
    // manifest.toml fetched at enable time; session-create has zero
    // network dependency on the manifest path.
    let enabled = state
        .services
        .meta
        .get_enabled_image(&image_uri)
        .await
        .map_err(|e| ApiError::Internal(format!("enabled_images lookup: {e}")))?
        .ok_or_else(|| {
            ApiError::BadRequest(format!(
                "image `{image_uri}` is not enabled. \
                 Operators enable images via POST /api/enabled-images \
                 (or the dashboard's Settings → Images panel) before \
                 sessions can reference them."
            ))
        })?;
    let manifest: ImageManifest = toml::from_str(&enabled.manifest_toml).map_err(|e| {
        ApiError::Internal(format!(
            "stored manifest for {image_uri} failed to parse: {e}"
        ))
    })?;

    // Validate the harness against the Postgres `harness_packs`
    // registry. Stage B2 dropped the host-resident scan — every harness
    // is registry-backed; the host-agent pulls the OCI artifact on
    // first use and assembles a per-session substrate that gets
    // mounted at `/run/engram/harnesses`. We resolve to the registry
    // URI now so the SandboxSpec carries it down to the host-agent.
    let harness_pack_uri: Option<String> = if let HarnessSpec::Builtin { name } = &req.harness {
        let pack = state
            .services
            .meta
            .get_harness_pack(name)
            .await
            .map_err(|e| ApiError::Internal(format!("harness lookup: {e}")))?;
        match pack {
            Some(p) => Some(p.registry_uri),
            None => {
                let available: Vec<String> = state
                    .services
                    .meta
                    .list_harness_packs()
                    .await
                    .map(|v| v.into_iter().map(|p| p.name).collect())
                    .unwrap_or_default();
                return Err(ApiError::BadRequest(format!(
                    "no harness `{name}` registered. Available: {available:?}. \
                     Register via `engram harness add --name {name} --push <registry/repo:tag>`."
                )));
            }
        }
    } else {
        None
    };

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
    apply_secrets_to_env(
        &mut spec_env,
        &secret_bundle,
        manifest.secret_mode,
        session_id,
    );

    // Per-request secrets that don't appear in the image manifest's
    // schema (e.g. harness-supplied creds like CLAUDE_CODE_OAUTH_TOKEN
    // — the demo image declares no `[secrets.*]` blocks) wouldn't
    // otherwise reach the harness env: `SecretStore::resolve` only
    // iterates the schema, so override values for unknown names get
    // silently dropped. Fold them in here, overwriting any same-key
    // value injected by the manifest's defaults / the secret store —
    // the user explicitly typed a value into the dashboard for *this*
    // session, that intent dominates. The broker-mode gate upstream
    // (line ~204) guarantees overrides only show up under
    // `SecretMode::Literal`, so passing them through verbatim is
    // safe — the operator asserted the value, no proxy substitution
    // contract is implied.
    if manifest.secret_mode == engram_core::types::image::SecretMode::Literal {
        if let Some(overrides) = req.secrets.as_ref() {
            for (name, value) in overrides {
                spec_env.insert(name.clone(), value.clone());
            }
        }
    }

    let agent_for_session = resolve_harness(
        &state,
        &req.harness,
        session_id,
        req.prompt.as_deref(),
        &spec_env,
    )?;

    // Network policy: start from the image manifest's `[network]`
    // block and auto-augment `allow_hosts` with the Git workspace's
    // host (so `git clone` can reach it). FC's net layer translates
    // this into per-VM iptables rules; VZ logs a warn-once because
    // its Apple-NAT path is opaque.
    let mut network = manifest.network.clone();
    if let WorkspaceSpec::Git { url, .. } = &req.workspace {
        if let Some(host) = host_from_url(url) {
            if !network.allow_hosts.iter().any(|h| h == &host) {
                network.allow_hosts.push(host);
            }
        }
    }

    // Clone the env + network policy before they're moved into the
    // VmSpec — the post-create proxy registration needs to look up
    // placeholders by name and build the network allow-list. The
    // borrow checker would otherwise rightly complain.
    let spec_env_for_proxy = spec_env.clone();
    let network_for_proxy = network.clone();

    // The image URI is the host-agent's pull target. The rootfs is
    // pulled from the registry on first use and cached locally; there's
    // no filesystem-resident "rootfs_source" anymore. The harness pack
    // — when one is requested — flows through the same OCI puller and
    // gets assembled into a per-session substrate by the host-agent.
    let vm_spec = VmSpec {
        image: image_uri.clone(),
        rootfs_source: None,
        image_uri: Some(image_uri.clone()),
        harness_pack_uri,
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
        harness_substrate: None,
        network,
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
    // Register with the egress proxy so outbound HTTPS from this
    // sandbox gets the right per-secret substitution + the right
    // network.allow_hosts policy. Skipped when the proxy isn't
    // running (`--egress-proxy-port=0`), or when the backend has
    // no guest IP for this sandbox (process backend, VZ, etc.).
    if let Some(proxy) = state.services.egress_proxy.as_ref() {
        if let Some(guest_ip_str) = state.services.sandbox.guest_ip(sandbox_id).await {
            if let Ok(guest_ip) = guest_ip_str.parse::<std::net::Ipv4Addr>() {
                let mut secrets_for_proxy = Vec::new();
                for (name, resolved) in &secret_bundle.secrets {
                    let allow = engram_egress_proxy::HostList::from_manifest(
                        &resolved.schema.allow_hosts,
                        &resolved.schema.allow_host_patterns,
                    )
                    .ok();
                    let placeholder = spec_env_for_proxy.get(name).cloned();
                    let (Some(allow), Some(placeholder)) = (allow, placeholder) else {
                        continue;
                    };
                    secrets_for_proxy.push(engram_egress_proxy::SecretEntry {
                        placeholder,
                        real_value: resolved.value.clone(),
                        allow,
                    });
                }
                let network_allow = engram_egress_proxy::HostList::from_manifest(
                    &network_for_proxy.allow_hosts,
                    &network_for_proxy.allow_host_patterns,
                )
                .unwrap_or_default();
                proxy.registry.register(engram_egress_proxy::SessionState {
                    session_id,
                    guest_ip,
                    network_allow,
                    secrets: secrets_for_proxy,
                });
            }
        }
    }
    // Bind the routing in the hub *before* the agent has a chance
    // to dial. `backend.create()` returns with the sandbox ready
    // to accept exec but the agent (if any) NOT yet spawned.
    state.harness_hub.bind_session(session_id, sandbox_id);

    // -------- 6. Materialize workspace --------
    // Runs while the row is still `Pending`. A failure here marks
    // the row Failed before the user ever sees Active — a clear
    // "git clone broke" beats a green session with no checkout.
    if let Err(e) =
        crate::workspace::materialize(&*state.services.sandbox, sandbox_id, &req.workspace).await
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
        return Err(ApiError::Internal(format!(
            "workspace materialization: {e}"
        )));
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
        if let Some(proxy) = state.services.egress_proxy.as_ref() {
            proxy.registry.unregister(id);
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
/// Resolve a session's [`HarnessSpec`] against the host's harness
/// registry, building the [`AgentSpec`] the backend will spawn at
/// `start_agent` time. `None` is returned for `HarnessSpec::None` —
/// the sandbox boots with no agent, leaving the user to drive it
/// via the in-browser shell or `engram exec`.
///
/// Stage B2 made harnesses registry-only: the host-agent pulls the
/// pack on first use into its content-addressable cache and assembles
/// a per-session ext4 substrate that the VM mounts at
/// `/run/engram/harnesses`. Two argv shapes depending on backend:
/// - `HostTcp` (Process): `argv[0]` is the guest path; the host-agent
///   rewrites it to its local cache path before launching, and the
///   harness dials TCP loopback on `--connect`.
/// - `Vsock` (FC/VZ): `argv[0]` is `/run/engram/harnesses/<name>`,
///   exec'd inside the VM over the substrate mount; the harness
///   dials AF_VSOCK on `--vsock-host`.
pub(crate) fn resolve_harness(
    state: &SharedState,
    spec: &HarnessSpec,
    session_id: SessionId,
    initial_prompt: Option<&str>,
    base_env: &HashMap<String, String>,
) -> Result<Option<engram_core::types::sandbox::AgentSpec>, ApiError> {
    let name = match spec {
        HarnessSpec::None => return Ok(None),
        HarnessSpec::Builtin { name } => name,
    };

    let mut env: HashMap<String, String> = base_env.clone();
    env.insert("ENGRAM_SESSION_ID".into(), session_id.to_string());
    if let Some(prompt) = initial_prompt {
        env.insert("ENGRAM_INITIAL_PROMPT".into(), prompt.to_string());
    }

    let guest_path = crate::harness_paths::guest_argv0(name);
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
            vec![
                guest_path,
                "--connect".into(),
                addr.to_string(),
                "--session-id".into(),
                session_id.to_string(),
            ]
        }
        engram_core::traits::HarnessDial::Vsock => {
            let port = engram_harness_proto::HARNESS_VSOCK_PORT;
            vec![
                guest_path,
                "--vsock-host".into(),
                port.to_string(),
                "--session-id".into(),
                session_id.to_string(),
            ]
        }
    };
    Ok(Some(engram_core::types::sandbox::AgentSpec { argv, env }))
}

#[cfg(test)]
mod tests {
    use super::host_from_url;

    #[test]
    fn host_from_url_https() {
        assert_eq!(
            host_from_url("https://github.com/cortex/api.git").as_deref(),
            Some("github.com"),
        );
    }

    #[test]
    fn host_from_url_http() {
        assert_eq!(
            host_from_url("http://gitea.local:3000/me/repo").as_deref(),
            Some("gitea.local"),
        );
    }

    #[test]
    fn host_from_url_ssh_form() {
        assert_eq!(
            host_from_url("git@github.com:cortex/api.git").as_deref(),
            Some("github.com"),
        );
    }

    #[test]
    fn host_from_url_user_at_host_form() {
        assert_eq!(
            host_from_url("alice@gitea.example.com:repo.git").as_deref(),
            Some("gitea.example.com"),
        );
    }

    #[test]
    fn host_from_url_unhandleable_returns_none() {
        // Plain `file://` and bare paths can't be allowlisted by host.
        assert_eq!(host_from_url("/srv/git/repo.git"), None);
    }
}
