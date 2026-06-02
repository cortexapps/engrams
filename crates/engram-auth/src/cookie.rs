//! `CookieSession` — resolves the HttpOnly session cookie set after an OIDC
//! login. Looks up the cookie token's hash in the `web_sessions` store; the
//! store rejects expired rows and inactive (deprovisioned) users, so a hit
//! here is a live, authorized human session.

use std::sync::Arc;

use async_trait::async_trait;
use engram_core::traits::WebSessionStore;

use crate::error::AuthError;
use crate::session::hash_token;
use crate::verify::{IdentityVerifier, Verified, VerifyInput};

/// Name of the session cookie. Shared with the coordinator's Set-Cookie path.
pub const SESSION_COOKIE: &str = "engram_session";

pub struct CookieSession {
    web_sessions: Arc<dyn WebSessionStore>,
}

impl CookieSession {
    pub fn new(web_sessions: Arc<dyn WebSessionStore>) -> Self {
        Self { web_sessions }
    }
}

#[async_trait]
impl IdentityVerifier for CookieSession {
    async fn verify(&self, input: &VerifyInput) -> Result<Option<Verified>, AuthError> {
        let Some(token) = input.cookie(SESSION_COOKIE) else {
            return Ok(None);
        };
        let hash = hash_token(token);
        match self.web_sessions.lookup_web_session(&hash).await? {
            Some((_session, user)) => Ok(Some(Verified::Principal(user.to_principal()))),
            // Cookie present but unknown/expired/inactive → not authenticated
            // via this mechanism. Fall through; the chain's terminal miss
            // yields the 401 that bounces the browser to /auth/login.
            None => Ok(None),
        }
    }

    fn name(&self) -> &'static str {
        "cookie-session"
    }
}
