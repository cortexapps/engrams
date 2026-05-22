use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use engram_core::traits::{SecretBundle, SecretContext};
use engram_core::types::sandbox::{CpuLimit, DiskLimit, MemoryLimit, SandboxSpec as VmSpec};
use engram_core::types::session::{split_image_ref, HarnessSpec, ImageRef};
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

fn short_hash(s: &str) -> String {
    // Stable 8-char tag of `s`, just for placeholder uniqueness. Not
    // a security primitive — placeholders aren't sensitive.
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:08x}", h.finish() & 0xffff_ffff)
}

/// Seal the per-request `secrets` overrides under the deployment KEK
/// and persist them keyed by `session_id`. Plaintext is JSON-encoded.
/// Caller is expected to have validated `overrides` is non-empty.
pub(crate) async fn persist_session_secrets(
    state: &SharedState,
    session_id: SessionId,
    overrides: &HashMap<String, String>,
) -> Result<(), ApiError> {
    let plaintext = serde_json::to_vec(overrides)
        .map_err(|e| ApiError::Internal(format!("serialize session secrets: {e}")))?;
    let cipher = engram_crypto::CredCipher::new(state.services.kek.as_ref());
    let sealed = cipher
        .seal(&plaintext)
        .await
        .map_err(|e| ApiError::Internal(format!("seal session secrets: {e}")))?;
    let row = engram_core::types::registry::SessionSecrets {
        session_id,
        wrapped_dek: sealed.wrapped_dek,
        nonce: sealed.nonce.to_vec(),
        ciphertext: sealed.ciphertext,
        key_id: sealed.key_id,
        created_at: chrono::Utc::now(),
    };
    state.services.meta.upsert_session_secrets(row).await?;
    Ok(())
}

/// Re-resolve the image manifest's `[secrets.*]` schema against the
/// deployment's `SecretStore` for a session, returning the env map
/// the harness should boot with. Looks up the session's image_uri
/// in `enabled_images`, parses the cached manifest, calls
/// `services.secrets.resolve(...)` with the same `(repo, tag)`
/// context the create path used, and applies the bundle into a
/// fresh env map via the same `apply_secrets_to_env` rules.
///
/// Re-resolving (vs. snapshotting at create) means an operator who
/// rotates a secret in the deployment's secret store mid-session
/// sees the new value land on the next resume — surprising the
/// harness with a stale value would be a bug, not the fix. The
/// caller is expected to fold per-request overrides
/// (CLAUDE_CODE_OAUTH_TOKEN, etc.) on top via [`load_session_secrets`].
pub(crate) async fn resolve_manifest_secrets(
    state: &SharedState,
    session: &Session,
) -> Result<HashMap<String, String>, ApiError> {
    let enabled = state
        .services
        .meta
        .get_enabled_image(&session.image)
        .await?
        .ok_or_else(|| {
            ApiError::Internal(format!(
                "session image `{}` is no longer enabled; can't re-resolve manifest secrets",
                session.image,
            ))
        })?;
    let manifest: ImageManifest = toml::from_str(&enabled.manifest_toml).map_err(|e| {
        ApiError::Internal(format!(
            "stored manifest for `{}` failed to parse: {e}",
            session.image,
        ))
    })?;
    let (repo, tag) = split_image_ref(&session.image);
    let secret_ctx = SecretContext {
        repo,
        image_tag: tag,
    };
    // Resume doesn't accept new per-request overrides — those came in
    // at create time and ride through via load_session_secrets.
    let bundle: SecretBundle = state
        .services
        .secrets
        .resolve(&secret_ctx, &manifest.secrets, None)
        .await
        .map_err(|e| ApiError::Internal(format!("secret resolution: {e}")))?;
    let mut env: HashMap<String, String> = manifest.env.clone();
    apply_secrets_to_env(&mut env, &bundle, manifest.secret_mode, session.id);
    Ok(env)
}

/// Reverse of [`persist_session_secrets`]: fetch the sealed row,
/// open it under the deployment KEK, and return the original
/// `(name, value)` map. `Ok(None)` when no row exists (the session
/// either had no overrides at create or was created before this
/// persistence path landed).
pub(crate) async fn load_session_secrets(
    state: &SharedState,
    session_id: SessionId,
) -> Result<Option<HashMap<String, String>>, ApiError> {
    let row = state.services.meta.get_session_secrets(session_id).await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let nonce: [u8; 12] = row
        .nonce
        .as_slice()
        .try_into()
        .map_err(|_| ApiError::Internal("session_secrets.nonce wrong length".into()))?;
    let sealed = engram_crypto::SealedCred {
        wrapped_dek: row.wrapped_dek,
        nonce,
        ciphertext: row.ciphertext,
        key_id: row.key_id,
    };
    let cipher = engram_crypto::CredCipher::new(state.services.kek.as_ref());
    let plaintext = cipher
        .open(&sealed)
        .await
        .map_err(|e| ApiError::Internal(format!("open session secrets: {e}")))?;
    let map: HashMap<String, String> = serde_json::from_slice(&plaintext)
        .map_err(|e| ApiError::Internal(format!("deserialize session secrets: {e}")))?;
    Ok(Some(map))
}

#[derive(Deserialize)]
pub struct CreateSessionRequest {
    /// Which baked image to boot. Required.
    pub image: ImageRef,
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
    /// `"warm"` if the session was satisfied by a pre-restored
    /// warm-pool slot, `"cold"` if it took the full create path.
    /// Used both by the dashboard (badge in session detail) and by
    /// the metrics wrapper to label `engram_session_boot_seconds`
    /// without re-running the scheduling decision.
    pub kind: &'static str,
}

pub async fn create_session(
    state: State<SharedState>,
    req: Json<CreateSessionRequest>,
) -> Result<(StatusCode, Json<CreateSessionResponse>), ApiError> {
    let start = std::time::Instant::now();
    let result = create_session_inner(state, req).await;
    let elapsed = start.elapsed().as_secs_f64();
    let outcome = match &result {
        Ok(_) => "success",
        Err(ApiError::BadRequest(_)) => "bad_request",
        Err(ApiError::NotFound(_)) => "image_not_enabled",
        Err(ApiError::Unavailable(_)) => "scheduling_rejected",
        Err(_) => "internal",
    };
    // Read kind off the success response; on error we don't know
    // which path was attempted (warm-lease may have errored before
    // we knew to fall through, or the spec failed validation
    // pre-scheduling), so label as `unknown`.
    let kind = match &result {
        Ok((_, body)) => body.kind,
        Err(_) => "unknown",
    };
    metrics::histogram!(
        crate::metrics::SESSION_BOOT_SECONDS,
        "phase" => "total",
        "outcome" => outcome,
        "kind" => kind,
    )
    .record(elapsed);
    metrics::counter!(
        crate::metrics::SESSION_CREATE_TOTAL,
        "outcome" => outcome,
    )
    .increment(1);
    result
}

async fn create_session_inner(
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

    // ADR 0005: workspace comes from the bake image; the platform
    // never clones a repo or materializes anything itself.
    let spec = SessionSpec {
        image: req.image.clone(),
        harness: req.harness.clone(),
        user_id: req.user_id,
    };

    // -------- 3. Mint a SessionId (no DB write yet) --------
    //
    // The row is persisted in Phase 5, AFTER scheduling succeeds —
    // so a scheduling failure (no capacity, image-not-found, host
    // crash mid-create) returns 503 with no zombie row. Phase 4
    // builds vm_spec / harness args using this id, which has to
    // exist before the sandbox does because those args ride INTO
    // the sandbox's env at create time.
    //
    // TODO(self-healing reconciler): when transient-capacity blips
    // become a real pattern (warm pools, multi-region cold-start),
    // flip this around: persist a Pending row with the full
    // vm_spec captured, and have a background reconciler retry
    // scheduling against later-arriving capacity. Today the
    // session row carries `image + harness + user_id` only —
    // not enough to reconstruct a vm_spec — so the v1 fix is
    // "fail loud, let the user retry" rather than "queue and
    // retry in the background". Schema work to capture the
    // full spec is the gating change.
    let session_id = SessionId::new();

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
    // Captured-but-deferred: the env injection is pure CPU and has to
    // happen before vm_spec construction, but the DB persist depends
    // on the session row existing (FK from session_secrets.session_id
    // → sessions.id). Under the post-aa794b8 flow the row isn't
    // written until *after* scheduling succeeds, so we defer the
    // persist call to that point. See `deferred_session_secrets`
    // below for where it actually fires.
    let deferred_session_secrets: Option<HashMap<String, String>> =
        if manifest.secret_mode == engram_core::types::image::SecretMode::Literal {
            if let Some(overrides) = req.secrets.as_ref() {
                for (name, value) in overrides {
                    spec_env.insert(name.clone(), value.clone());
                }
                // Persist sealed under the deployment KEK so resume can
                // rebuild the post-resume harness's launch env. Without
                // this, an idle-then-active transition (auto-resume on
                // the next prompt) respawns the harness child with no
                // secret env — Claude prompts the user to log in again.
                // Skipped when overrides is empty so we don't create
                // empty rows.
                if overrides.is_empty() {
                    None
                } else {
                    Some(overrides.clone())
                }
            } else {
                None
            }
        } else {
            None
        };

    // The host-agent's `pooled_backend` reads this env hint to pick
    // the substrate's mount-root subdir name. Without it, it falls
    // back to the URI's last path segment (`harness-claude` for
    // `localhost:5001/cortex/harness-claude:v1`) — but the in-VM
    // bootstrap exec's `/run/engram/harnesses/<name>/harness` using
    // the user-supplied harness name (`claude`), so the two paths
    // diverge and the harness binary doesn't get found at boot. The
    // env hint pins the host-agent to the canonical name.
    if let HarnessSpec::Builtin { name } = &req.harness {
        spec_env.insert("ENGRAM_SESSION_HARNESS_NAME".into(), name.clone());
    }

    let agent_for_session = resolve_harness(
        &state,
        &req.harness,
        session_id,
        req.prompt.as_deref(),
        &spec_env,
    )?;

    // Network policy: image manifest's `[network]` block, verbatim.
    // ADR 0005 retired the platform's git workspace (no clone URL to
    // auto-allowlist anymore); agents that need GitHub egress
    // declare `[network] allow_hosts = ["api.github.com"]` in their
    // image manifest like any other dependency.
    let network = manifest.network.clone();

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
        harness_pack_uri: harness_pack_uri.clone(),
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
        // ADR 0007 Phase 5: PooledBackend populates this from the
        // image bundle after the image-cache resolve, so we leave
        // it None at session-create. Sessions whose bundle carries
        // a canonical pick up the canonical mmap at restore time.
    };

    // -------- 5. Schedule + create the sandbox --------
    //
    // No row exists yet. If scheduling fails the caller gets a 503
    // and Postgres is untouched; a retry can succeed once capacity
    // recovers. See TODO at the SessionId mint for the eventual
    // self-healing reconciler design that would persist Pending and
    // retry in the background instead.
    let ctx = ScheduleContext {
        repo: &image_repo,
        image_version: &image_tag,
        prefer_snapshot_id: None,
        memory_mib: Some(vm_spec.memory.max_mib),
        // ADR 0015 M5: gate placement on hosts that have prefetched
        // this image. `enabled.manifest_digest` is the same digest
        // hosts include in their heartbeat's `ready_images`.
        required_image_digest: Some(engram_protocol::heartbeat::ManifestDigest::new(
            &enabled.manifest_digest,
        )),
    };

    // ADR 0015 M5: cold-create only, gated on host readiness. Sessions
    // land on a host whose latest heartbeat reported the manifest
    // digest above in `ready_images`. No ready host → 503
    // `image_not_ready` (distinct from "no capacity").
    let (host_id, sandbox_id) = match state.host_registry.create_for_session(&ctx, vm_spec).await {
        Ok((host_id, id)) => (host_id, id),
        Err(e) => {
            tracing::warn!(
                image_uri = %req.image,
                error = %e,
                "session create rejected at scheduling; returning 503, no row persisted",
            );
            return Err(match e {
                engram_core::SandboxError::ImageNotReady(digest) => ApiError::Unavailable(format!(
                    "image `{}` (digest {digest}) has not been prefetched by any host yet. \
                     Hosts diff `enabled_images` against their local NVMe chunk cache on each \
                     heartbeat (~5s) and prefetch missing images; retry shortly. \
                     If this persists, check BlobStorage egress and host-agent logs for \
                     `image prefetch` lines.",
                    req.image
                )),
                other => ApiError::Unavailable(format!(
                    "no host has capacity for this session right now: {other}. \
                     Retry shortly; capacity-fit recovers as hosts register or sessions drain."
                )),
            });
        }
    };

    // Atomic insert: row exists only once we have host_id + sandbox_id
    // bound. If THIS step fails, the sandbox is already running and
    // would orphan — tear it down before bubbling. Note the
    // `services.host.destroy` is best-effort; a failure here leaves
    // a sandbox running on the host until its owning host-agent's
    // reconcile pass flips it.
    if let Err(e) = state
        .services
        .meta
        .create_session_active(session_id, spec, host_id, sandbox_id)
        .await
    {
        tracing::error!(
            session_id = %session_id,
            sandbox_id = %sandbox_id,
            host_id = %host_id,
            error = %e,
            "session row insert failed after sandbox create; tearing sandbox down",
        );
        if let Err(de) = state.services.host.destroy(sandbox_id).await {
            tracing::error!(
                session_id = %session_id,
                sandbox_id = %sandbox_id,
                error = %de,
                "sandbox teardown after insert failure also failed — host reconcile will GC",
            );
        }
        return Err(e.into());
    }

    // Now that the session row exists, the FK on session_secrets is
    // satisfiable. Best-effort: a failure here just means resume
    // re-prompts for credentials (the harness still gets them on the
    // initial boot via vm_spec.env).
    if let Some(overrides) = deferred_session_secrets {
        if let Err(e) = persist_session_secrets(&state, session_id, &overrides).await {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "session secrets persistence failed; resume will lose secrets",
            );
        }
    }

    state.registry.bind(session_id, sandbox_id);
    // ADR 0006: ship per-session egress policy to the host-agent
    // that owns this sandbox. The host-agent applies it to its
    // local proxy registry; the WS frame and any subsequent
    // start_agent request are serialised on the same connection so
    // the policy is live before the harness can make egress calls.
    //
    // Build the egress policy from the resolved guest IP. None when
    // the backend has no guest IP for this sandbox (process backend,
    // VZ in some configs) — fine; host-agents without a proxy
    // ignore the policy field.
    let egress_policy = if let Some(guest_ip_str) = state.services.host.guest_ip(sandbox_id).await {
        if let Ok(guest_ip) = guest_ip_str.parse::<std::net::Ipv4Addr>() {
            let mut secrets = Vec::new();
            for (name, resolved) in &secret_bundle.secrets {
                let Some(placeholder) = spec_env_for_proxy.get(name).cloned() else {
                    continue;
                };
                secrets.push(engram_core::types::egress::EgressSecretEntry {
                    placeholder,
                    real_value: resolved.value.clone(),
                    allow_hosts: resolved.schema.allow_hosts.clone(),
                    allow_host_patterns: resolved.schema.allow_host_patterns.clone(),
                });
            }
            Some(engram_core::types::egress::SessionEgressPolicy {
                session_id,
                sandbox_id,
                guest_ip,
                network_allow_hosts: network_for_proxy.allow_hosts.clone(),
                network_allow_host_patterns: network_for_proxy.allow_host_patterns.clone(),
                secrets,
                secret_mode: manifest.secret_mode,
            })
        } else {
            None
        }
    } else {
        None
    };
    // Bind the routing on the host that owns this sandbox *before*
    // the agent has a chance to dial. `backend.create()` returns with
    // the sandbox ready to accept exec but the agent (if any) NOT
    // yet spawned. HostClient dispatches by sandbox_id so this routes
    // to the right host (local in mode=all, WS in mode=coordinator).
    state
        .services
        .host
        .bind_session(session_id, sandbox_id)
        .await;

    // -------- 6. Start the agent --------
    //
    // ADR 0015 M1: every cold-create session goes through
    // `start_agent`, including `harness: none`. Agentd's
    // SpawnHarness handler treats empty argv as a readiness
    // probe (no child spawned, no error), so the same call
    // serves both cases — and as a side-effect "session is
    // Active" now actually implies "agentd is reachable on
    // vsock", which makes the early-eof race that the no-
    // harness path used to produce structurally impossible.
    let agent = agent_for_session.unwrap_or_else(|| engram_core::types::sandbox::AgentSpec {
        argv: Vec::new(),
        env: std::collections::HashMap::new(),
    });
    let policy = egress_policy.unwrap_or_else(|| {
        // No guest IP yet → synthesize an unspecified-IP policy.
        // host-agents without a live proxy treat policy application
        // as a no-op; the agent can refine on a later
        // `apply_egress_policy` once the IP shows up.
        engram_core::types::egress::SessionEgressPolicy {
            session_id,
            sandbox_id,
            guest_ip: std::net::Ipv4Addr::UNSPECIFIED,
            network_allow_hosts: network_for_proxy.allow_hosts.clone(),
            network_allow_host_patterns: network_for_proxy.allow_host_patterns.clone(),
            secrets: Vec::new(),
            secret_mode: manifest.secret_mode,
        }
    });
    if let Err(e) = state
        .services
        .host
        .start_agent(sandbox_id, agent, policy)
        .await
    {
        tracing::error!(
            %session_id,
            %sandbox_id,
            %host_id,
            error = %e,
            "start_agent failed; marking session Failed",
        );
        let _ = state
            .services
            .meta
            .set_session_status(session_id, SessionStatus::Failed)
            .await;
        state.services.host.unbind_session(session_id).await;
        return Err(e.into());
    }
    // Row was inserted as Active in Phase 5; no status update needed.
    // Still emit a Pending→Active StatusChanged so SSE subscribers
    // see the lifecycle event (`Pending` is the implicit pre-insert
    // state from the API caller's point of view, even though we
    // never persisted it).
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
            kind: "cold",
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
        if let Err(e) = state.services.host.destroy(sandbox_id).await {
            tracing::warn!(
                session_id = %id,
                sandbox_id = %sandbox_id,
                error = %e,
                "sandbox destroy failed during session delete; continuing",
            );
        }
        // ADR 0006: the host-agent unregisters its local proxy
        // entry as part of `destroy`. No coordinator-side cleanup.
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
    state.services.host.unbind_session(id).await;
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
    let argv = match state.services.host.harness_dial() {
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
