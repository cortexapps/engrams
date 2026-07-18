//! ADR 0098 R2: the API-driven workload — the Router the ADR promised.
//!
//! Pre-R2 the workload called store/core fns directly (0/17 HTTP routes,
//! 0/~69 gRPC RPCs driven — the audit's finding). Now each replica's REAL
//! surface is exercised, over the SAME `SharedState` the drivers run on.
//!
//! ## real-wire vs handler-direct (the honesty table)
//!
//! | verb                    | path          | why |
//! |-------------------------|---------------|-----|
//! | admin pause / resume    | **real-wire** HTTP | a route exists (`/api/v1/admin/sessions/:id/{pause,resume}`); `tower::ServiceExt::oneshot` on the actual `api::router` runs the bearer middleware + extractors |
//! | harness-event ingest    | **real-wire** HTTP | host→coord ingestion route (`/api/v1/sessions/:id/harness-events`); the `harness_idle` emitter that lets Flow Evicting fire at CI step counts (#775) |
//! | create_session          | **handler-direct** gRPC | ADR 0051 retired the REST create — it is gRPC-ONLY, no axum route to oneshot. Driven through the real `AppSessionService` tonic impl (auth + convert.rs + `*_core`) |
//! | send_prompt             | handler-direct gRPC | as above |
//! | resume                  | handler-direct gRPC | as above |
//! | delete_session          | handler-direct gRPC | as above |
//! | admin_drain_host        | handler-direct gRPC | `AppFleetService` — the operator-drain that exercises Evacuating |
//!
//! The gRPC verbs are invoked with a constructed `tonic::Request` carrying
//! the bearer in metadata — the real auth check runs — but WITHOUT a
//! socket/HTTP2 frame: a live tonic server+client would need real IO and
//! break the paused-clock, current-thread determinism the whole sim rests
//! on. That transport hop is the only thing skipped; the request decode,
//! the auth gate, the proto↔domain conversion, and the transport-agnostic
//! core are all the production code.
//!
//! Auth reuses the e2e/test-auth shape: the axum surface is configured
//! with a bearer (`SIM_HTTP_TOKEN`) and every HTTP request carries it; the
//! app-gRPC `BearerAuth` is configured with `SIM_GRPC_TOKEN` and every
//! `tonic::Request` carries it. Neither path bypasses validation inside a
//! handler.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request};
use chrono::{DateTime, Utc};
use engram_coordinator::grpc_app::auth::BearerAuth;
use engram_coordinator::grpc_app::{AppFleetService, AppSessionService};
use engram_coordinator::state::SharedState;
use engram_core::{HostId, SandboxId, SessionId};
use engram_harness_proto::HarnessEvent;
use engram_protocol::app;
use engram_protocol::app::fleet_service_server::FleetService as _;
use engram_protocol::app::session_service_server::SessionService as _;
use tower::ServiceExt as _;

/// The axum internal-surface bearer (mirrors the deployment `auth_tokens`).
pub const SIM_HTTP_TOKEN: &str = "sim-http-bearer";
/// The app-gRPC bearer (mirrors the deployment `app_grpc_tokens`).
pub const SIM_GRPC_TOKEN: &str = "sim-grpc-bearer";

/// Drain any tasks a handler DETACHED (`session_ops::enqueue` spawns
/// `drive_claimed` off the caller's future, ADR 0079 — resume/delete/evict
/// take this path) to a FIXED point, so the op's mutating work lands
/// inside the step rather than interleaving nondeterministically with the
/// next pick. A fixed yield budget on the current-thread runtime is itself
/// deterministic; in the fault-free case the detached op completes well
/// within it. (Under an rpc-hang the detached op parks on a timer — it
/// then sits at the same fixed point every replay, still deterministic,
/// and the healed quiescence pass converges it like any other op.)
pub async fn drain_detached() {
    for _ in 0..256 {
        tokio::task::yield_now().await;
    }
}

fn grpc_req<T>(inner: T) -> tonic::Request<T> {
    let mut req = tonic::Request::new(inner);
    req.metadata_mut().insert(
        "authorization",
        format!("Bearer {SIM_GRPC_TOKEN}")
            .parse()
            .expect("ascii bearer"),
    );
    req
}

fn session_service(state: &SharedState) -> AppSessionService {
    AppSessionService {
        state: state.clone(),
        auth: Arc::new(BearerAuth::new(vec![SIM_GRPC_TOKEN.to_string()])),
    }
}

fn fleet_service(state: &SharedState) -> AppFleetService {
    AppFleetService {
        state: state.clone(),
        auth: Arc::new(BearerAuth::new(vec![SIM_GRPC_TOKEN.to_string()])),
    }
}

/// The acked outcome of an API create — the model oracle (R2 auditor) is
/// fed ONLY by these acks.
#[derive(Debug, Clone)]
pub struct CreatedAck {
    pub session_id: SessionId,
    pub status: String,
    pub image_version: String,
}

/// gRPC `create_session` (handler-direct). Returns the acked session on
/// success; a lost/failed create is `None` (legitimately absent from the
/// model — a lost-response op leaves no ack).
pub async fn api_create(state: &SharedState, image_uri: &str) -> Option<CreatedAck> {
    let req = grpc_req(app::CreateSessionRequest {
        image_uri: image_uri.to_string(),
        mode: "dev_vm".to_string(),
        ..Default::default()
    });
    match session_service(state).create_session(req).await {
        Ok(resp) => {
            let r = resp.into_inner();
            Some(CreatedAck {
                session_id: r.session_id.parse().ok()?,
                status: r.status,
                image_version: r.image_version,
            })
        }
        Err(_) => None,
    }
}

/// gRPC `send_prompt` (handler-direct). `true` iff the coordinator acked.
pub async fn api_prompt(state: &SharedState, session_id: SessionId, prompt_id: &str) -> bool {
    let req = grpc_req(app::SendPromptRequest {
        session_id: session_id.to_string(),
        text: "sim: a prompt".to_string(),
        prompt_id: prompt_id.to_string(),
    });
    session_service(state).send_prompt(req).await.is_ok()
}

/// gRPC `resume` (handler-direct).
pub async fn api_resume(state: &SharedState, session_id: SessionId) -> bool {
    let req = grpc_req(app::ResumeRequest {
        session_id: session_id.to_string(),
    });
    session_service(state).resume(req).await.is_ok()
}

/// gRPC `delete_session` (handler-direct). `true` iff destroy was acked.
pub async fn api_delete(state: &SharedState, session_id: SessionId) -> bool {
    let req = grpc_req(app::DeleteSessionRequest {
        session_id: session_id.to_string(),
    });
    session_service(state).delete_session(req).await.is_ok()
}

/// gRPC `admin_drain_host` (handler-direct, FleetService) — the operator
/// drain that moves bound sessions into Evacuating.
pub async fn api_drain_host(state: &SharedState, host_id: HostId) -> bool {
    let req = grpc_req(app::AdminDrainHostRequest {
        host_id: host_id.to_string(),
    });
    fleet_service(state).admin_drain_host(req).await.is_ok()
}

/// The axum internal surface for `state`, built fresh (cheap) so a
/// replica's Router always reflects its current `SharedState` (rebuilt on
/// restart).
fn http_router(state: &SharedState) -> axum::Router {
    engram_coordinator::api::router(state.clone())
}

fn bearer_post(uri: String, body: Body) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("authorization", format!("Bearer {SIM_HTTP_TOKEN}"))
        .header("content-type", "application/json")
        .body(body)
        .expect("build request")
}

/// Real-wire HTTP admin pause then resume of a live sandbox in place. A
/// session with no live sandbox 409s inside the handler — swallowed here.
pub async fn api_pause_then_resume(state: &SharedState, session_id: SessionId) {
    let router = http_router(state);
    let pause = bearer_post(
        format!("/api/v1/admin/sessions/{session_id}/pause"),
        Body::empty(),
    );
    let _ = router.clone().oneshot(pause).await;
    let resume = bearer_post(
        format!("/api/v1/admin/sessions/{session_id}/resume"),
        Body::empty(),
    );
    let _ = router.oneshot(resume).await;
}

/// Real-wire HTTP host-ingestion of a `harness_idle` event — the newest
/// event flips the session eviction-eligible, so the idle detector
/// nominates it once the TTL elapses (Flow Evicting, the #775 gap).
pub async fn api_emit_harness_idle(
    state: &SharedState,
    session_id: SessionId,
    sandbox_id: SandboxId,
    at: DateTime<Utc>,
) {
    let body = serde_json::json!({
        "sandbox_id": sandbox_id,
        "event": HarnessEvent::Idle,
        "at": at,
    });
    let req = bearer_post(
        format!("/api/v1/sessions/{session_id}/harness-events"),
        Body::from(serde_json::to_vec(&body).expect("serialize harness event")),
    );
    let _ = http_router(state).oneshot(req).await;
}
