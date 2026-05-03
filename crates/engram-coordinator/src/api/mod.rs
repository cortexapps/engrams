use axum::middleware;
use axum::routing::{delete, get, post};
use axum::Router;

use crate::state::SharedState;

pub mod auth;
mod checkpoint;
mod events;
mod exec;
mod harnesses;
mod health;
mod hosts;
mod images;
mod prompt;
mod registries;
mod sessions;
mod sessions_inspect;
mod shell;
mod snapshot;

pub fn router(state: SharedState) -> Router {
    // The protected sub-router gets the bearer-token layer. `/healthz`
    // is grafted on outside the layer so liveness probes (k8s, GCP
    // load balancers) don't have to be told a token. If we ever need
    // an authenticated `/readyz`, it lives on the protected side.
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
        .route("/sessions/:id/checkpoint", post(checkpoint::checkpoint))
        .route("/sessions/:id/prompt", post(prompt::prompt))
        .route("/sessions/:id/shell", get(shell::shell))
        .route("/sessions/:id/log", get(sessions_inspect::log))
        .route("/sessions/:id/diff", get(sessions_inspect::diff))
        .route("/sessions/:id/fork", post(sessions_inspect::fork))
        .route("/api/hosts/connect", get(hosts::connect))
        .route("/api/hosts", get(hosts::list))
        .route("/api/hosts/:id", get(hosts::get))
        .route("/api/hosts/:id/drain", post(hosts::drain))
        .route("/api/images", get(images::list_images))
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
        .layer(middleware::from_fn_with_state(
            auth_state,
            auth::require_bearer,
        ));

    Router::new()
        .route("/healthz", get(health::healthz))
        .merge(protected)
        .with_state(state)
}
