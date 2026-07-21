use axum::middleware;
use axum::routing::{get, post};
use axum::Router;

use crate::state::SharedState;

// `pub(crate)`: the live-migration regression test (issue #208) drives
// the real `teleport_session` handler to prove HTTP cancellation no
// longer abandons the verb.
pub(crate) mod admin;
pub mod auth;
pub(crate) mod enabled_images;
// `pub(crate)`: ADR 0051 — the app-gRPC services (`grpc_app/*`) call the
// transport-agnostic `*_core` fns extracted from these handlers.
pub(crate) mod events;
pub(crate) mod exec;
pub(crate) mod forge;
// ADR 0021 P1.5a retired `mod harnesses;` — the harness_packs
// registry doesn't exist anymore (the harness is an image property
// baked at image-bake time).
mod health;
// `pub`: ADR 0084 P1b live-PG tests (`enable_reuse_live_pg`,
// `enable_jobs_live_pg`) drive `claim_capture_job` directly (in-process,
// no HTTP) to simulate a host claiming + executing a capture job without
// standing up a real host-agent.
pub mod host_http;
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
pub(crate) mod write_files;

pub fn router(state: SharedState) -> Router {
    // ADR 0051: the web-facing (protected / session-scoped / member / admin /
    // auth-flow) human routers are GONE. The orchestrator owns all web traffic
    // and calls the coordinator exclusively over the app-gRPC surface
    // (`grpc_app/*`), which delegates to the transport-agnostic `*_core` fns.
    // Only the internal (host → coord) ingestion, the in-guest forge seam, the
    // admin pause/resume control plane, and health probes remain on HTTP.
    //
    // Production note: ENGRAM_AUTH_TOKENS must include the orchestrator's
    // bearer so its REST proxy calls to /admin/sessions/:id/pause and
    // /admin/sessions/:id/resume (the only remaining REST control verbs,
    // pending their gRPC promotion) are authenticated. The orchestrator sends
    // CONTROL_PLANE_BEARER on those requests; ensure both tokens agree at
    // deploy time.
    let auth_state = auth::AuthState::new(state.cfg.auth_tokens.clone());

    // ADR 0031 internal control-plane: host → coord ingestion. Machine
    // traffic authenticated by the deployment bearer (`require_bearer`).
    // pause/resume live here (not a human surface) behind the same bearer.
    let internal = Router::new()
        // ADR 0045 Phase F: freeze / unfreeze a microVM in place.
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
            "/hosts/:id/capture-jobs/:job_id/claim",
            post(host_http::claim_capture_job),
        )
        .route(
            "/hosts/:id/sessions/:session_id/sandboxes/:sandbox_id/ownership",
            get(host_http::sandbox_ownership),
        )
        // ADR 0090: the unknown-binding form (teardown reconciler).
        .route(
            "/hosts/:id/sandboxes/:sandbox_id/owner",
            get(host_http::sandbox_owner),
        )
        .route(
            "/hosts/:id/live-manifest",
            post(host_http::live_manifest_publish),
        )
        // WS4: the egress proxy re-mints a near-expiry inject credential.
        .route(
            "/hosts/:id/sessions/:session_id/inject/refresh",
            post(host_http::refresh_inject),
        )
        .route(
            "/sessions/:id/harness-events",
            post(host_http::harness_event_ingest),
        )
        .route(
            "/sessions/:id/integration-asset",
            post(host_http::integration_asset_ingest),
        )
        .layer(middleware::from_fn_with_state(
            auth_state,
            auth::require_bearer,
        ));

    // ADR 0023 in-session forge seam. Authenticated in-handler by
    // the per-session credential-broker token (not the deployment
    // bearer), so these live OUTSIDE the `internal` bearer layer — the
    // in-guest helper holds only its session-scoped token.
    let forge_seam =
        Router::new().route("/sessions/:id/git-credential", get(forge::git_credential));

    // `/healthz` + `/readyz` stay at root so k8s + the GCP LB health
    // checks don't have to know the `/api/v1` prefix.
    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .nest("/api/v1", forge_seam.merge(internal))
        .with_state(state)
}
