use axum::middleware;
use axum::routing::{delete, get, post};
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
pub mod principal;
pub(crate) mod prompt;
pub(crate) mod registries;
pub(crate) mod session_auth;
// `pub(crate)`: `evacuation::resolve_cold_boot_spec` (ADR 0028 Fix B)
// reuses `cold_boot_spec` / the resource helpers from outside `api`.
pub(crate) mod sessions;
pub(crate) mod sessions_inspect;
mod shell;
pub mod snapshot;
pub(crate) mod storage;
pub(crate) mod upload;

pub fn router(state: SharedState) -> Router {
    // The protected sub-router gets the bearer-token layer.
    // `/healthz` (liveness) and `/readyz` (readiness — pings
    // Postgres) are grafted on outside the layer so k8s and GCP LB
    // probes don't have to be told a token.
    let auth_state = auth::AuthState::new(state.cfg.auth_tokens.clone());

    // ADR 0031: per-session routes, owner-scoped by the `require_session_owner`
    // layer (a member may only touch their own sessions; others' ids → 404).
    let session_scoped = Router::new()
        .route(
            "/sessions/:id",
            get(sessions::get_session).delete(sessions::delete_session),
        )
        .route("/sessions/:id/exec", post(exec::exec))
        .route("/sessions/:id/exec/stream", post(exec::exec_stream))
        .route("/sessions/:id/events", get(events::events))
        // Static `from-path` segment takes priority over `:artifact_id`
        // (UUIDs, no clash).
        .route(
            "/sessions/:id/artifacts/:artifact_id",
            get(upload::serve_artifact),
        )
        .route(
            "/sessions/:id/artifacts/from-path",
            post(upload::create_from_path),
        )
        .route("/sessions/:id/snapshot", post(snapshot::snapshot))
        .route("/sessions/:id/resume", post(snapshot::resume))
        .route("/sessions/:id/local", delete(snapshot::evict_local))
        .route("/sessions/:id/prompt", post(prompt::prompt))
        .route("/sessions/:id/interrupt", post(interrupt::interrupt))
        .route("/sessions/:id/shell", get(shell::shell))
        .route("/sessions/:id/log", get(sessions_inspect::log))
        .route("/sessions/:id/cow-state", get(sessions_inspect::cow_state))
        .route(
            "/sessions/:id/checkpoints",
            get(sessions_inspect::checkpoints),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            principal::require_session_owner,
        ));

    // ADR 0031 MEMBER surface: any authenticated principal. `/sessions`
    // list+create (list self-scopes; create stamps the owner), the
    // owner-scoped per-session routes, self-service `/me*`, and GET
    // /enabled-images (needed by the create-session form).
    let member = Router::new()
        .route(
            "/sessions",
            get(sessions::list_sessions).post(sessions::create_session),
        )
        .merge(session_scoped)
        // Read-only list of enabled images — members pick one to launch.
        .route("/enabled-images", get(enabled_images::list_enabled_images))
        // ADR 0036: enable-job polling (progress bars). Read-only —
        // mutations (POST enable / retry) live on the admin router.
        .route("/enable-jobs", get(enabled_images::list_enable_jobs))
        .route("/enable-jobs/:id", get(enabled_images::get_enable_job))
        // ADR 0031 self-service.
        .route("/me", get(principal::me))
        .route("/me/claude-token", post(principal::save_claude_token))
        .route("/auth/logout", post(principal::logout));

    // ADR 0031 ADMIN surface: operator + global-config routes. Gated by the
    // `require_admin` route layer (the real authorization gate); the principal
    // it reads is inserted by `resolve_principal` (the outer layer below).
    let admin = Router::new()
        // Fleet.
        .route("/hosts", get(hosts::list))
        .route("/hosts/:id", get(hosts::get))
        .route("/hosts/:id/drain", post(hosts::drain))
        .route("/hosts/:id/cow-state", get(hosts::cow_state))
        // Storage.
        .route("/storage/summary", get(storage::summary))
        // Registries (global config).
        .route(
            "/registries",
            get(registries::list_registries).post(registries::add_registry),
        )
        .route("/registries/:host", delete(registries::delete_registry))
        // Enabled-image *mutations* (GET is on the member router).
        .route("/enabled-images", post(enabled_images::enable_image))
        .route(
            "/enabled-images/refresh",
            post(enabled_images::refresh_enabled_image),
        )
        .route(
            "/enabled-images/disable",
            post(enabled_images::disable_enabled_image),
        )
        // ADR 0036: re-queue a failed enable job.
        .route(
            "/enable-jobs/:id/retry",
            post(enabled_images::retry_enable_job),
        )
        // Operator admin triggers.
        .route(
            "/admin/reap-materialize-dir",
            post(admin::reap_materialize_dir),
        )
        .route("/admin/sessions/:id/flush-now", post(admin::flush_now))
        .route(
            "/admin/sessions/:id/evacuate",
            post(admin::evacuate_session),
        )
        .route(
            "/admin/sessions/:id/teleport",
            post(admin::teleport_session),
        )
        // ADR 0045 Phase F: freeze / unfreeze a microVM in place.
        .route("/admin/sessions/:id/pause", post(admin::pause_session))
        .route("/admin/sessions/:id/resume", post(admin::resume_session))
        .route("/admin/hosts/:id/cordon", post(admin::cordon_host))
        .route("/admin/hosts/:id/uncordon", post(admin::uncordon_host))
        .route("/admin/hosts/:id/drain", post(admin::drain_host))
        // ADR 0044 K4: fleet-demand signal for the node-pool autoscaler.
        .route("/admin/fleet/demand", get(admin::fleet_demand))
        .route("/admin/chunk-gc/dry-run", post(admin::chunk_gc_dry_run))
        .route("/admin/chunk-gc/sweep", post(admin::chunk_gc_sweep))
        .route("/admin/bundle-gc/dry-run", post(admin::bundle_gc_dry_run))
        .route("/admin/bundle-gc/sweep", post(admin::bundle_gc_sweep))
        .route(
            "/admin/snapshot-blob-gc/dry-run",
            post(admin::snapshot_blob_gc_dry_run),
        )
        .route(
            "/admin/snapshot-blob-gc/sweep",
            post(admin::snapshot_blob_gc_sweep),
        )
        .route(
            "/admin/chunk-gc/candidates",
            get(admin::chunk_gc_candidates),
        )
        // User administration.
        .route("/admin/users", get(principal::list_users))
        .route(
            "/admin/users/:id",
            axum::routing::patch(principal::patch_user),
        )
        .layer(middleware::from_fn(principal::require_admin));

    // ADR 0031: human traffic resolves a Principal via the verifier chain
    // (cookie → service-bearer → forward-auth → synthetic). This outer layer
    // runs first for both member + admin routes, inserting the principal that
    // `require_admin` (inner, admin-only) then reads.
    let protected = member.merge(admin).layer(middleware::from_fn_with_state(
        state.clone(),
        principal::resolve_principal,
    ));

    // ADR 0031 internal control-plane: host → coord ingestion. Machine
    // traffic authenticated by the deployment bearer (`require_bearer`),
    // NOT the human verifier chain — so the host-agent never needs a cookie
    // and a 401 here never tries to redirect a browser to /auth/login.
    let internal = Router::new()
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

    // ADR 0031 unauthenticated auth-flow entrypoints (you can't be authed to
    // log in). OIDC redirect + callback set/consume the session cookie.
    let auth_routes = Router::new()
        .route("/auth/login", get(principal::login))
        .route("/auth/callback", get(principal::callback));

    // ADR 0023 in-session forge seam. Authenticated in-handler by
    // the per-session credential-broker token (not the deployment
    // bearer), so these live OUTSIDE the `protected` layer — the
    // in-guest helper holds only its session-scoped token.
    let forge_seam = Router::new()
        .route("/sessions/:id/git-credential", get(forge::git_credential))
        .route(
            "/sessions/:id/pull-request",
            post(forge::create_pull_request),
        );

    // The whole HTTP API lives under `/api/v1`. The SPA owns the root
    // path namespace (`/`, `/sessions/:id`, `/settings/...`), so the
    // web reverse-proxy can split cleanly on `/api/` vs everything-else
    // and a browser deep-link to `/sessions/:id` no longer collides
    // with the `GET /sessions/:id` API route. `/healthz` + `/readyz`
    // stay at root so k8s + the GCP LB health checks don't have to know
    // the prefix (and they're liveness/readiness, not API surface).
    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .nest(
            "/api/v1",
            forge_seam
                .merge(internal)
                .merge(auth_routes)
                .merge(protected),
        )
        .with_state(state)
}
