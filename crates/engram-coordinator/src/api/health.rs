use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;

use crate::state::SharedState;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
}

/// Liveness probe — process is up and answering. No dependency
/// checks. K8s sends this on a tight loop; if it 503s, the pod is
/// killed.
pub async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[derive(Serialize)]
pub struct ReadyResponse {
    pub status: &'static str,
    pub version: &'static str,
    /// `Some(message)` if a dependency check failed. The LB uses the
    /// HTTP status (200 vs 503) to gate traffic; the body is for
    /// humans debugging an unhealthy replica.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Readiness probe — coordinator can serve real traffic. Pings the
/// Postgres-backed metadata store via `MetadataStore::ping` so a
/// replica that lost its DB connection drops out of the LB pool
/// instead of returning 503s to clients.
///
/// Deliberately does **not** check the connected-host count: a
/// coordinator should be `Ready` even before any FC host has dialed
/// in (so admin/UI routes work during bring-up). Session-create
/// against a coordinator with zero hosts naturally 503s downstream,
/// which is the right behavior.
pub async fn readyz(State(state): State<SharedState>) -> impl IntoResponse {
    match state.services.meta.ping().await {
        Ok(()) => (
            StatusCode::OK,
            Json(ReadyResponse {
                status: "ready",
                version: env!("CARGO_PKG_VERSION"),
                error: None,
            }),
        ),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ReadyResponse {
                status: "not_ready",
                version: env!("CARGO_PKG_VERSION"),
                error: Some(format!("metadata store: {e}")),
            }),
        ),
    }
}
