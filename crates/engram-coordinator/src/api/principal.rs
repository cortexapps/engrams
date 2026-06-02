//! ADR 0031 principal resolution + authorization extractors.
//!
//! The [`resolve_principal`] middleware runs the [`VerifierChain`] over each
//! request (cookie → service-bearer → forward-auth → synthetic) and stashes
//! the resolved [`Principal`] in request extensions. Handlers pull it out via
//! the [`CurrentUser`] extractor; admin-only routes are gated once by the
//! [`require_admin`] route layer (the real authorization gate). When no auth
//! runtime is configured (tests /
//! `AppState::new` without wiring), the layer injects a synthetic admin so
//! the suite runs authed-as-admin unchanged.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{FromRequestParts, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use base64::Engine;
use chrono::{Duration, Utc};
use engram_auth::cookie::SESSION_COOKIE;
use engram_auth::error::AuthError;
use engram_auth::{
    hash_token, mint_session_token, seal_user_token, AuthConfig, AuthMode, OidcAuthenticator,
    VerifiedEmail, VerifierChain, VerifyInput,
};
use engram_core::traits::{UserStore, WebSessionStore};
use engram_core::types::user::{Principal, Role, RoleSource, User, UserToken, WebSession};
use engram_core::UserId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::state::SharedState;

/// Short-lived cookie carrying the OIDC flow state (CSRF state + nonce + PKCE
/// verifier) between `/auth/login` and `/auth/callback`.
const OIDC_FLOW_COOKIE: &str = "engram_oidc_flow";

/// The authentication runtime held on `AppState.auth` (see its doc comment).
pub struct AuthRuntime {
    pub chain: VerifierChain,
    /// Present only in `oidc` mode; drives `/auth/login` + `/auth/callback`.
    pub oidc: Option<OidcAuthenticator>,
    pub users: Arc<dyn UserStore>,
    pub web_sessions: Arc<dyn WebSessionStore>,
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

/// Route-layer guard for `/sessions/:id*`: a member may only touch their own
/// sessions; another user's id (or an unknown one) returns 404 — the same
/// status, so it never confirms a session exists. Admins bypass. Applied once
/// to the session-scoped sub-router rather than checked in each handler.
pub async fn require_session_owner(
    State(state): State<SharedState>,
    req: Request,
    next: Next,
) -> Response {
    let Some(principal) = req.extensions().get::<Principal>().cloned() else {
        return ApiError::Unauthorized("authentication required".into()).into_response();
    };
    if principal.is_admin() {
        return next.run(req).await;
    }

    // Pull the `:id` path param (robust to routes that carry extra params,
    // e.g. `/sessions/:id/artifacts/:artifact_id`).
    let (mut parts, body) = req.into_parts();
    let session_id =
        axum::extract::Path::<std::collections::HashMap<String, String>>::from_request_parts(
            &mut parts, &state,
        )
        .await
        .ok()
        .and_then(|p| {
            p.0.get("id")
                .and_then(|s| s.parse::<engram_core::SessionId>().ok())
        });
    let req = Request::from_parts(parts, body);

    let Some(session_id) = session_id else {
        return ApiError::BadRequest("missing session id".into()).into_response();
    };

    match state.services.meta.get_session(session_id).await {
        Ok(s) if s.user_id.as_deref() == Some(principal.user_id.to_string().as_str()) => {
            next.run(req).await
        }
        // Owner mismatch OR not found → 404 (don't reveal another user's
        // session even exists).
        _ => ApiError::NotFound("session not found".into()).into_response(),
    }
}

// ---------------------------------------------------------------------
// Endpoint handlers (ADR 0031): /me, /me/claude-token, /auth/*, /admin/users
// ---------------------------------------------------------------------

fn map_auth_err(e: AuthError) -> ApiError {
    match e {
        AuthError::Inactive => ApiError::Forbidden("account is deactivated".into()),
        AuthError::Verify(m) => ApiError::Unauthorized(m),
        AuthError::Store(e) => e.into(),
        AuthError::Config(m) => ApiError::Internal(format!("auth config: {m}")),
        AuthError::Http(m) => ApiError::Unavailable(format!("identity provider: {m}")),
    }
}

/// Build a `Set-Cookie` value. `max_age` of `None` clears the cookie.
fn set_cookie(name: &str, value: &str, secure: bool, max_age: Option<i64>) -> String {
    let mut c = format!("{name}={value}; HttpOnly; SameSite=Lax; Path=/");
    if secure {
        c.push_str("; Secure");
    }
    match max_age {
        Some(secs) => c.push_str(&format!("; Max-Age={secs}")),
        None => c.push_str("; Max-Age=0"),
    }
    c
}

#[derive(Serialize)]
pub struct MeResponse {
    pub email: String,
    pub display_name: Option<String>,
    pub role: String,
    pub is_admin: bool,
    /// Whether the user has saved a Claude Code OAuth token (drives the
    /// never-prompt create-session gating). Never returns the token itself.
    pub has_claude_token: bool,
    /// Whether interactive sign-out is meaningful. Only in OIDC mode does the
    /// app own a revocable session cookie; behind a forward-auth proxy (IAP)
    /// the proxy re-authenticates every request, and dev synthetic-admin has
    /// no session — so the web hides the Sign-out control in those modes.
    pub can_sign_out: bool,
}

/// `GET /me` — the current principal + whether they have a saved Claude token.
pub async fn me(
    State(state): State<SharedState>,
    CurrentUser(p): CurrentUser,
) -> Result<Json<MeResponse>, ApiError> {
    let has_claude_token = match &state.auth {
        Some(rt) => rt
            .users
            .get_user_token(p.user_id, UserToken::KIND_CLAUDE_OAUTH)
            .await
            .map(|t| t.is_some())
            .unwrap_or(false),
        None => false,
    };
    let can_sign_out = state
        .auth
        .as_ref()
        .map(|rt| rt.config.mode == AuthMode::Oidc)
        .unwrap_or(false);
    Ok(Json(MeResponse {
        email: p.email,
        display_name: p.display_name,
        role: p.role.as_str().to_string(),
        is_admin: p.role.is_admin(),
        has_claude_token,
        can_sign_out,
    }))
}

#[derive(Deserialize)]
pub struct SaveTokenRequest {
    pub token: String,
}

/// `POST /me/claude-token` — seal + store the user's Claude Code OAuth token.
pub async fn save_claude_token(
    State(state): State<SharedState>,
    CurrentUser(p): CurrentUser,
    Json(req): Json<SaveTokenRequest>,
) -> Result<StatusCode, ApiError> {
    let rt = state
        .auth
        .as_ref()
        .ok_or_else(|| ApiError::Unsupported("auth is not configured".into()))?;
    let token = req.token.trim();
    if token.is_empty() {
        return Err(ApiError::BadRequest("token must not be empty".into()));
    }
    let sealed = seal_user_token(
        state.services.kek.as_ref(),
        p.user_id,
        UserToken::KIND_CLAUDE_OAUTH,
        token,
    )
    .await
    .map_err(|e| ApiError::Internal(format!("seal token: {e}")))?;
    rt.users.upsert_user_token(sealed).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /auth/logout` — revoke the session and clear the cookie.
pub async fn logout(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if let Some(rt) = &state.auth {
        if let Some(token) = verify_input(&headers).cookie(SESSION_COOKIE) {
            let _ = rt.web_sessions.revoke_web_session(&hash_token(token)).await;
        }
    }
    let secure = state
        .auth
        .as_ref()
        .map(|rt| rt.config.cookie_secure)
        .unwrap_or(false);
    let clear = set_cookie(SESSION_COOKIE, "", secure, None);
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, clear)]).into_response()
}

/// `GET /auth/login` — start the OIDC flow (or bounce to `/` when behind a
/// proxy / in synthetic mode, where there's no interactive login).
pub async fn login(State(state): State<SharedState>) -> Result<Response, ApiError> {
    let Some(rt) = &state.auth else {
        return Ok(Redirect::to("/").into_response());
    };
    match rt.config.mode {
        AuthMode::Oidc => {
            let oidc = rt
                .oidc
                .as_ref()
                .ok_or_else(|| ApiError::Internal("oidc mode but no authenticator".into()))?;
            let start = oidc.start().await.map_err(map_auth_err)?;
            // Stash {state, nonce, pkce} in a short-lived cookie to validate
            // the callback; values are single-use and re-validated there.
            let flow = serde_json::json!({
                "state": start.state,
                "nonce": start.nonce,
                "pkce": start.pkce_verifier,
            });
            let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(flow.to_string());
            let cookie = set_cookie(
                OIDC_FLOW_COOKIE,
                &encoded,
                rt.config.cookie_secure,
                Some(600),
            );
            Ok((
                StatusCode::FOUND,
                [
                    (header::LOCATION, start.authorize_url),
                    (header::SET_COOKIE, cookie),
                ],
            )
                .into_response())
        }
        // Forward-auth / synthetic: already authenticated upstream (or no
        // auth) — nothing to log into.
        AuthMode::ForwardAuth | AuthMode::None => Ok(Redirect::to("/").into_response()),
    }
}

#[derive(Deserialize)]
pub struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
}

#[derive(Deserialize)]
struct OidcFlow {
    state: String,
    nonce: String,
    pkce: String,
}

/// `GET /auth/callback` — validate the OIDC redirect, mint a session cookie.
pub async fn callback(
    State(state): State<SharedState>,
    Query(params): Query<CallbackParams>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let rt = state
        .auth
        .as_ref()
        .ok_or_else(|| ApiError::Unsupported("auth is not configured".into()))?;
    let oidc = rt
        .oidc
        .as_ref()
        .ok_or_else(|| ApiError::Internal("oidc callback but no authenticator".into()))?;

    let code = params
        .code
        .ok_or_else(|| ApiError::BadRequest("missing code".into()))?;
    let returned_state = params
        .state
        .ok_or_else(|| ApiError::BadRequest("missing state".into()))?;

    // Recover + validate the flow cookie (CSRF: returned state must match).
    let cookie_val = verify_input(&headers)
        .cookie(OIDC_FLOW_COOKIE)
        .map(str::to_string)
        .ok_or_else(|| ApiError::BadRequest("missing oidc flow cookie".into()))?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cookie_val.as_bytes())
        .map_err(|_| ApiError::BadRequest("malformed flow cookie".into()))?;
    let flow: OidcFlow = serde_json::from_slice(&decoded)
        .map_err(|_| ApiError::BadRequest("malformed flow cookie".into()))?;
    if flow.state != returned_state {
        return Err(ApiError::BadRequest("oidc state mismatch".into()));
    }

    let verified = oidc
        .callback(&code, &flow.pkce, &flow.nonce)
        .await
        .map_err(map_auth_err)?;
    let principal = rt
        .chain
        .provision(VerifiedEmail {
            email: verified.email,
            display_name: verified.display_name,
        })
        .await
        .map_err(map_auth_err)?;

    // Mint the web session.
    let token = mint_session_token();
    let now = Utc::now();
    rt.web_sessions
        .create_web_session(WebSession {
            token_hash: hash_token(&token),
            user_id: principal.user_id,
            created_at: now,
            expires_at: now + Duration::hours(rt.config.session_ttl_hours.max(1)),
            last_seen_at: now,
        })
        .await?;

    let session_cookie = set_cookie(
        SESSION_COOKIE,
        &token,
        rt.config.cookie_secure,
        Some(rt.config.session_ttl_hours.max(1) * 3600),
    );
    let clear_flow = set_cookie(OIDC_FLOW_COOKIE, "", rt.config.cookie_secure, None);

    // Emit BOTH Set-Cookie headers via append — an axum `[(header, value)]`
    // array collapses same-key entries (the second clobbers the first), which
    // would drop the session cookie entirely and leave the user in a login
    // loop. append keeps both.
    let mut resp = Redirect::to("/").into_response();
    let headers = resp.headers_mut();
    for cookie in [session_cookie, clear_flow] {
        let value = HeaderValue::from_str(&cookie)
            .map_err(|e| ApiError::Internal(format!("set-cookie: {e}")))?;
        headers.append(header::SET_COOKIE, value);
    }
    Ok(resp)
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

/// `GET /admin/users` — list users. Admin-gated by the `require_admin` route
/// layer (these routes live on the admin sub-router).
pub async fn list_users(
    State(state): State<SharedState>,
) -> Result<Json<Vec<UserSummary>>, ApiError> {
    let rt = state
        .auth
        .as_ref()
        .ok_or_else(|| ApiError::Unsupported("auth is not configured".into()))?;
    let users = rt.users.list_users().await?;
    Ok(Json(users.iter().map(UserSummary::from).collect()))
}

#[derive(Deserialize)]
pub struct PatchUserRequest {
    pub role: Option<String>,
    pub active: Option<bool>,
}

/// `PATCH /admin/users/:id` — set role and/or active (admin only). A manual
/// role set here is authoritative and survives SCIM/JIT re-provisioning.
/// Deactivating a user also revokes their live sessions.
pub async fn patch_user(
    State(state): State<SharedState>,
    Path(id): Path<Uuid>,
    Json(req): Json<PatchUserRequest>,
) -> Result<Json<UserSummary>, ApiError> {
    let rt = state
        .auth
        .as_ref()
        .ok_or_else(|| ApiError::Unsupported("auth is not configured".into()))?;
    let uid = UserId(id);
    let mut user = rt.users.get_user(uid).await?;
    if let Some(role_str) = req.role {
        let role = Role::parse(&role_str)
            .ok_or_else(|| ApiError::BadRequest(format!("unknown role {role_str:?}")))?;
        user = rt
            .users
            .set_user_role(uid, role, RoleSource::Manual)
            .await?;
    }
    if let Some(active) = req.active {
        user = rt.users.set_user_active(uid, active).await?;
        if !active {
            // Deprovision: drop their live sessions immediately.
            let _ = rt.web_sessions.revoke_all_for_user(uid).await;
        }
    }
    Ok(Json(UserSummary::from(&user)))
}

#[cfg(test)]
mod tests {
    use super::*;

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
