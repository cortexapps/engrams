//! ADR 0031 principal resolution + authorization extractors.
//!
//! The [`resolve_principal`] middleware runs the [`VerifierChain`] over each
//! request (service-bearer → forward-auth) and stashes the resolved
//! [`Principal`] in request extensions. Handlers pull it out via the
//! [`CurrentUser`] extractor; admin-only routes are gated once by the
//! [`require_admin`] route layer. When no auth runtime is configured (tests /
//! `AppState::new` without wiring), the layer injects a synthetic admin so
//! the suite runs authed-as-admin unchanged.
//!
//! ADR 0039 Task 31: users table dropped. Cookie/synthetic/OIDC removed from
//! the chain. AuthRuntime no longer holds UserStore or WebSessionStore.

use std::collections::HashMap;

use async_trait::async_trait;
use axum::extract::{FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use engram_auth::error::AuthError;
use engram_auth::{AuthConfig, OidcAuthenticator, VerifierChain, VerifyInput};
use engram_core::types::user::{Principal, Role, User};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::SharedState;

/// The authentication runtime held on `AppState.auth` (see its doc comment).
pub struct AuthRuntime {
    pub chain: VerifierChain,
    /// Present only in `oidc` mode; stub — OIDC flow is removed in Task 31
    /// but the field is kept so Task 32 can do a clean deletion in one pass.
    pub oidc: Option<OidcAuthenticator>,
    pub config: AuthConfig,
}

/// The synthetic admin principal used when no auth runtime is configured
/// (tests / un-wired `AppState`). Matches what `engram_auth::SyntheticAdmin`
/// would produce, so the authed-as-admin behaviour is identical whether the
/// runtime is absent or running in `none` mode.
fn synthetic_admin() -> Principal {
    Principal {
        user_id: engram_auth::SYNTHETIC_USER_ID.into(),
        email: "dev@engram.local".to_string(),
        display_name: Some("Local Admin".to_string()),
        role: Role::Admin,
        active: true,
    }
}

/// Parse a `Cookie:` header into a name → value map.
fn parse_cookies(headers: &HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for value in headers.get_all(axum::http::header::COOKIE).iter() {
        let Ok(s) = value.to_str() else { continue };
        for pair in s.split(';') {
            let pair = pair.trim();
            if let Some((k, v)) = pair.split_once('=') {
                out.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }
    out
}

/// Build a framework-free [`VerifyInput`] from request headers + cookies.
pub fn verify_input(headers: &HeaderMap) -> VerifyInput {
    let mut hmap = HashMap::new();
    for (name, value) in headers.iter() {
        if let Ok(v) = value.to_str() {
            // Last-wins is fine; verifiers read single-valued headers.
            hmap.insert(name.as_str().to_ascii_lowercase(), v.to_string());
        }
    }
    VerifyInput {
        headers: hmap,
        cookies: parse_cookies(headers),
    }
}

/// Middleware: resolve the request's `Principal` and insert it into
/// extensions, or short-circuit with 401/403.
pub async fn resolve_principal(
    State(state): State<SharedState>,
    mut req: Request,
    next: Next,
) -> Response {
    let principal = match &state.auth {
        // No runtime → synthetic admin (tests / no-auth dev).
        None => synthetic_admin(),
        Some(rt) => {
            let input = verify_input(req.headers());
            match rt.chain.resolve(&input).await {
                Ok(Some(p)) => p,
                Ok(None) => {
                    return ApiError::Unauthorized("authentication required".into())
                        .into_response();
                }
                Err(AuthError::Inactive) => {
                    return ApiError::Forbidden("account is deactivated".into()).into_response();
                }
                Err(e) => {
                    tracing::warn!(error = %e, "principal resolution failed");
                    return ApiError::Unauthorized("authentication failed".into()).into_response();
                }
            }
        }
    };
    req.extensions_mut().insert(principal);
    next.run(req).await
}

/// Extractor for the authenticated principal. Pulls what [`resolve_principal`]
/// inserted; a missing principal means the route wasn't behind the layer (a
/// wiring bug) → 401.
#[derive(Clone, Debug)]
pub struct CurrentUser(pub Principal);

impl std::ops::Deref for CurrentUser {
    type Target = Principal;
    fn deref(&self) -> &Principal {
        &self.0
    }
}

#[async_trait]
impl<S: Send + Sync> FromRequestParts<S> for CurrentUser {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Principal>()
            .cloned()
            .map(CurrentUser)
            .ok_or_else(|| ApiError::Unauthorized("no authenticated principal".into()))
    }
}

/// Route-layer guard for admin-only endpoints (the real authorization gate;
/// the web only hides tabs). Applied once via `middleware::from_fn` to the
/// admin sub-router rather than repeated as a per-handler extractor — the
/// principal it reads is the one [`resolve_principal`] (an outer layer)
/// already inserted.
pub async fn require_admin(req: Request, next: Next) -> Response {
    let is_admin = req
        .extensions()
        .get::<Principal>()
        .map(Principal::is_admin)
        .unwrap_or(false);
    if is_admin {
        next.run(req).await
    } else {
        ApiError::Forbidden("admin role required".into()).into_response()
    }
}

/// Route-layer guard for `/sessions/:id*`. ADR 0039 Task 31: `user_id` column
/// dropped from sessions. The orchestrator owns authz; any authenticated
/// principal may access any session on the coordinator.
pub async fn require_session_owner(
    State(_state): State<SharedState>,
    req: Request,
    next: Next,
) -> Response {
    // ADR 0039 Task 31: user_id column dropped. The orchestrator owns authz;
    // any authenticated principal may access any session on the coordinator.
    next.run(req).await
}

// ---------------------------------------------------------------------
// Endpoint handlers (ADR 0031): /me, /me/claude-token, /auth/*, /admin/users
// ---------------------------------------------------------------------

#[derive(Serialize)]
pub struct MeResponse {
    pub email: String,
    pub display_name: Option<String>,
    pub role: String,
    pub is_admin: bool,
    /// ADR 0039 Task 31: user token storage removed. Always false.
    pub has_claude_token: bool,
    /// ADR 0039 Task 31: OIDC session removed. Always false.
    pub can_sign_out: bool,
}

/// `GET /me` — the current principal.
pub async fn me(
    State(_state): State<SharedState>,
    CurrentUser(p): CurrentUser,
) -> Result<Json<MeResponse>, ApiError> {
    Ok(Json(MeResponse {
        email: p.email,
        display_name: p.display_name,
        role: p.role.as_str().to_string(),
        is_admin: p.role.is_admin(),
        // ADR 0039 Task 31: users table dropped; token storage removed.
        has_claude_token: false,
        can_sign_out: false,
    }))
}

#[derive(Deserialize)]
pub struct SaveTokenRequest {
    pub token: String,
}

/// `POST /me/claude-token` — ADR 0039 Task 31: user token storage removed.
pub async fn save_claude_token(
    State(_state): State<SharedState>,
    CurrentUser(_p): CurrentUser,
    Json(_req): Json<SaveTokenRequest>,
) -> Result<StatusCode, ApiError> {
    Err(ApiError::Unsupported(
        "user token storage removed in ADR 0039 Task 31".into(),
    ))
}

/// `POST /auth/logout` — ADR 0039 Task 31: web_sessions table dropped; no-op.
pub async fn logout(State(_state): State<SharedState>, _headers: HeaderMap) -> Response {
    // ADR 0039 Task 31: web_sessions table dropped; nothing to revoke.
    StatusCode::NO_CONTENT.into_response()
}

/// `GET /auth/login` — ADR 0039 Task 31: OIDC flow removed; redirect to root.
pub async fn login(State(_state): State<SharedState>) -> Result<Response, ApiError> {
    // ADR 0039 Task 31: OIDC flow removed; redirect to root.
    Ok(Redirect::to("/").into_response())
}

#[derive(Deserialize)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
}

/// `GET /auth/callback` — ADR 0039 Task 31: OIDC callback removed.
pub async fn callback(
    State(_state): State<SharedState>,
    Query(_params): Query<CallbackParams>,
    _headers: HeaderMap,
) -> Result<Response, ApiError> {
    Err(ApiError::Unsupported(
        "OIDC callback removed in ADR 0039 Task 31".into(),
    ))
}

#[derive(Serialize)]
pub struct UserSummary {
    pub id: Uuid,
    pub email: String,
    pub display_name: Option<String>,
    pub role: String,
    pub role_source: String,
    pub active: bool,
}

impl From<&User> for UserSummary {
    fn from(u: &User) -> Self {
        Self {
            id: u.id.as_uuid(),
            email: u.email.clone(),
            display_name: u.display_name.clone(),
            role: u.role.as_str().to_string(),
            role_source: u.role_source.as_str().to_string(),
            active: u.active,
        }
    }
}

/// `GET /admin/users` — ADR 0039 Task 31: users table dropped; returns empty list.
pub async fn list_users(
    State(_state): State<SharedState>,
) -> Result<Json<Vec<UserSummary>>, ApiError> {
    Ok(Json(Vec::new()))
}

#[derive(Deserialize)]
pub struct PatchUserRequest {
    pub role: Option<String>,
    pub active: Option<bool>,
}

/// `PATCH /admin/users/:id` — ADR 0039 Task 31: user management removed.
pub async fn patch_user(
    State(_state): State<SharedState>,
    Path(_id): Path<Uuid>,
    Json(_req): Json<PatchUserRequest>,
) -> Result<Json<UserSummary>, ApiError> {
    Err(ApiError::Unsupported(
        "user management removed in ADR 0039 Task 31".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram_core::UserId;

    fn parts_with(principal: Option<Principal>) -> Parts {
        let (mut parts, _) = axum::http::Request::builder()
            .body(())
            .unwrap()
            .into_parts();
        if let Some(p) = principal {
            parts.extensions.insert(p);
        }
        parts
    }

    fn principal(role: Role) -> Principal {
        Principal {
            user_id: UserId::new(),
            email: "u@example.com".into(),
            display_name: None,
            role,
            active: true,
        }
    }

    #[tokio::test]
    async fn current_user_requires_an_injected_principal() {
        let mut parts = parts_with(None);
        let err = CurrentUser::from_request_parts(&mut parts, &())
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
    }

    /// Drive `require_admin` through a minimal router, injecting a principal
    /// via an outer layer the way `resolve_principal` would.
    async fn require_admin_status(principal: Option<Principal>) -> StatusCode {
        use axum::body::Body;
        use axum::routing::get;
        use axum::Router;
        use tower::ServiceExt;

        async fn ok() -> &'static str {
            "ok"
        }
        let app = Router::new()
            .route("/admin", get(ok))
            .layer(axum::middleware::from_fn(require_admin))
            .layer(axum::middleware::from_fn(
                move |mut req: Request, next: Next| {
                    let p = principal.clone();
                    async move {
                        if let Some(p) = p {
                            req.extensions_mut().insert(p);
                        }
                        next.run(req).await
                    }
                },
            ));
        app.oneshot(Request::get("/admin").body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn require_admin_admits_admin_and_rejects_others() {
        assert_eq!(
            require_admin_status(Some(principal(Role::Admin))).await,
            StatusCode::OK
        );
        assert_eq!(
            require_admin_status(Some(principal(Role::Member))).await,
            StatusCode::FORBIDDEN
        );
        // Missing principal (a wiring bug behind resolve_principal) is treated
        // as non-admin, not admitted.
        assert_eq!(require_admin_status(None).await, StatusCode::FORBIDDEN);
    }
}
