use axum::middleware;
use axum::routing::{delete, get, post};
use axum::Router;

use crate::state::SharedState;

mod admin;
pub mod auth;
mod enabled_images;
mod events;
mod exec;
mod harnesses;
mod health;
mod host_http;
mod hosts;
mod prompt;
mod registries;
mod sessions;
mod sessions_inspect;
mod shell;
pub mod snapshot;

pub fn router(state: SharedState) -> Router {
    // The protected sub-router gets the bearer-token layer.
    // `/healthz` (liveness) and `/readyz` (readiness — pings
    // Postgres) are grafted on outside the layer so k8s and GCP LB
    // probes don't have to be told a token.
    let auth_state = auth::AuthState::new(state.cfg.auth_tokens.clone());
    let protected = Router::new()
        .route(
            "/sessions",
            get(sessions::list_sessions).post(sessions::create_session),
        )
        .route(
            "/sessions/:id",
            get(sessions::get_session).delete(sessions::delete_session),
        )
        .route("/sessions/:id/exec", post(exec::exec))
        .route("/sessions/:id/exec/stream", post(exec::exec_stream))
        .route("/sessions/:id/events", get(events::events))
        .route("/sessions/:id/snapshot", post(snapshot::snapshot))
        .route("/sessions/:id/resume", post(snapshot::resume))
        .route("/sessions/:id/local", delete(snapshot::evict_local))
        .route("/sessions/:id/prompt", post(prompt::prompt))
        .route("/sessions/:id/shell", get(shell::shell))
        .route("/sessions/:id/log", get(sessions_inspect::log))
        // ADR 0016 Phase A: per-session COW diagnostic.
        .route("/sessions/:id/cow-state", get(sessions_inspect::cow_state))
        // ADR 0013 host → coord HTTP endpoints. The old
        // `/api/hosts/connect` WS handler has been retired —
        // host-agents register over HTTP, heartbeat over HTTP,
        // forward harness events over HTTP, and the coord
        // dispatches back to them over gRPC.
        .route("/api/hosts/register", post(host_http::register))
        .route("/api/hosts/:id/heartbeat", post(host_http::heartbeat))
        .route(
            "/api/hosts/:id/auth/resolve-registry",
            post(host_http::resolve_registry_auth),
        )
        .route(
            "/api/hosts/:id/idle-eviction-candidates",
            post(host_http::idle_eviction_candidates),
        )
        .route(
            "/sessions/:id/harness-events",
            post(host_http::harness_event_ingest),
        )
        .route("/api/hosts", get(hosts::list))
        .route("/api/hosts/:id", get(hosts::get))
        .route("/api/hosts/:id/drain", post(hosts::drain))
        // ADR 0016 Phase A: per-host COW diagnostic.
        .route("/api/hosts/:id/cow-state", get(hosts::cow_state))
        .route(
            "/api/harnesses",
            get(harnesses::list_harnesses).post(harnesses::add_harness),
        )
        .route("/api/harnesses/:name", delete(harnesses::delete_harness))
        .route(
            "/api/registries",
            get(registries::list_registries).post(registries::add_registry),
        )
        .route("/api/registries/:host", delete(registries::delete_registry))
        .route(
            "/api/enabled-images",
            get(enabled_images::list_enabled_images).post(enabled_images::enable_image),
        )
        .route(
            "/api/enabled-images/refresh",
            post(enabled_images::refresh_enabled_image),
        )
        .route(
            "/api/enabled-images/disable",
            post(enabled_images::disable_enabled_image),
        )
        .route(
            "/api/admin/reap-materialize-dir",
            post(admin::reap_materialize_dir),
        )
        .layer(middleware::from_fn_with_state(
            auth_state,
            auth::require_bearer,
        ));

    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .merge(protected)
        .with_state(state)
}
