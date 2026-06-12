use axum::middleware;
use axum::routing::{get, post};
use axum::Router;

use crate::state::SharedState;

pub(crate) mod admin;
pub mod auth;
pub(crate) mod enabled_images;
pub(crate) mod events;
pub(crate) mod exec;
pub(crate) mod forge;
// ADR 0021 P1.5a retired `mod harnesses;` — the harness_packs
// registry doesn't exist anymore (the harness is an image property
// baked at image-bake time).
mod health;
mod host_http;
pub(crate) mod hosts;
pub(crate) mod interrupt;
pub(crate) mod prompt;
pub(crate) mod registries;
pub(crate) mod session_auth;
// `pub(crate)`: `evacuation::resolve_cold_boot_spec` (ADR 0028 Fix B)
// reuses `cold_boot_spec` / the resource helpers from outside `api`.
pub(crate) mod sessions;
pub(crate) mod sessions_inspect;
pub mod snapshot;
pub(crate) mod storage;
pub(crate) mod upload;

pub fn router(state: SharedState) -> Router {
    // ADR 0039 Task 32: the protected/session_scoped/member/admin/auth_routes
    // web-facing routers are removed. The orchestrator owns web traffic and
    // calls the coordinator exclusively via the app-gRPC surface. Only the
    // internal (host → coord) and forge-seam (in-guest) routers remain here
    // behind the deployment bearer.
    //
    // Production note: ENGRAM_AUTH_TOKENS must include the orchestrator's
    // bearer so the orchestrator's REST proxy calls to
    // /admin/sessions/:id/pause and /admin/sessions/:id/resume
    // (the only remaining REST proxies, pending their gRPC promotion) are
    // authenticated. The orchestrator sends CONTROL_PLANE_BEARER on those
    // requests; ensure both tokens agree at deploy time.
    let auth_state = auth::AuthState::new(state.cfg.auth_tokens.clone());

    // ADR 0031 internal control-plane: host → coord ingestion. Machine
    // traffic authenticated by the deployment bearer (`require_bearer`).
    // ADR 0039 Task 31: pause/resume moved here from the protected router.
    // ADR 0039 Task 32: this is now the only non-health, non-forge HTTP surface.
    let internal = Router::new()
        // ADR 0045 Phase F: freeze / unfreeze a microVM in place.
        // Stays on the internal bearer-auth router (ADR 0039 Task 31).
        // In prod: ENGRAM_AUTH_TOKENS must include the orchestrator's bearer.
        .route("/admin/sessions/:id/pause", post(admin::pause_session))
        .route("/admin/sessions/:id/resume", post(admin::resume_session))
        .route("/hosts/register", post(host_http::register))
        .route("/hosts/:id/heartbeat", post(host_http::heartbeat))
        .route("/hosts/forge", post(forge::forge_forward))
        .route("/hosts/upload", post(upload::upload_forward))
        .route(
            "/hosts/:id/auth/resolve-registry",
            post(host_http::resolve_registry_auth),
        )
        .route(
            "/hosts/:id/sessions/:session_id/sandboxes/:sandbox_id/ownership",
            get(host_http::sandbox_ownership),
        )
        .route(
            "/hosts/:id/idle-eviction-candidates",
            post(host_http::idle_eviction_candidates),
        )
        .route(
            "/hosts/:id/live-manifest",
            post(host_http::live_manifest_publish),
        )
        .route(
            "/sessions/:id/harness-events",
            post(host_http::harness_event_ingest),
        )
        .layer(middleware::from_fn_with_state(
            auth_state,
            auth::require_bearer,
        ));

    // ADR 0023 in-session forge seam. Authenticated in-handler by
    // the per-session credential-broker token (not the deployment
    // bearer), so these live OUTSIDE the `internal` bearer layer — the
    // in-guest helper holds only its session-scoped token.
    let forge_seam = Router::new()
        .route("/sessions/:id/git-credential", get(forge::git_credential))
        .route(
            "/sessions/:id/pull-request",
            post(forge::create_pull_request),
        );

    // `/healthz` + `/readyz` stay at root so k8s + the GCP LB health
    // checks don't have to know the `/api/v1` prefix.
    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .nest("/api/v1", forge_seam.merge(internal))
        .with_state(state)
}
