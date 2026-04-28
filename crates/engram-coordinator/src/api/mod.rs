use axum::middleware;
use axum::routing::{delete, get, post};
use axum::Router;

use crate::state::SharedState;

pub mod auth;
mod events;
mod exec;
mod health;
mod hosts;
mod sessions;
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
        .route("/sessions/:id/migrate", post(sessions::migrate))
        .route("/api/hosts/connect", get(hosts::connect))
        .route("/api/hosts", get(hosts::list))
        .route("/api/hosts/:id", get(hosts::get))
        .route("/api/hosts/:id/drain", post(hosts::drain))
        .layer(middleware::from_fn_with_state(
            auth_state,
            auth::require_bearer,
        ));

    Router::new()
        .route("/healthz", get(health::healthz))
        .merge(protected)
        .with_state(state)
}
