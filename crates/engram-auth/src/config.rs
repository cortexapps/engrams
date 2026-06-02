//! Auth configuration + the [`build_chain`] factory. The coordinator's CLI
//! populates [`AuthConfig`]; this crate owns the shape so the chain assembly
//! stays here (and the IdP-specific values — Rippling, GCP IAP — are pure
//! config, never code).

use std::sync::Arc;

use engram_core::traits::{UserStore, WebSessionStore};

use crate::bearer::ServiceBearer;
use crate::chain::VerifierChain;
use crate::cookie::CookieSession;
use crate::forward::ForwardAuthVerifier;
use crate::synthetic::SyntheticAdmin;
use crate::verify::IdentityVerifier;

/// How the deployment authenticates humans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthMode {
    /// Coordinator runs the OIDC Authorization-Code + PKCE flow itself.
    Oidc,
    /// A trusted edge proxy (GCP IAP, Cloudflare Access, oauth2-proxy)
    /// authenticated the user and forwards a signed JWT.
    ForwardAuth,
    /// No SSO configured → synthetic admin (dev).
    None,
}

impl AuthMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Oidc => "oidc",
            Self::ForwardAuth => "forward-auth",
            Self::None => "none",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "oidc" => Some(Self::Oidc),
            "forward-auth" | "forward_auth" => Some(Self::ForwardAuth),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// OIDC client config (Authorization-Code + PKCE). Agnostic — point `issuer`
/// at any OIDC provider.
#[derive(Clone, Debug)]
pub struct OidcConfig {
    /// Issuer base URL; discovery is `{issuer}/.well-known/openid-configuration`.
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    /// The coordinator's `/api/v1/auth/callback` absolute URL.
    pub redirect_url: String,
    /// Requested scopes; `openid email profile` is the sensible default.
    pub scopes: Vec<String>,
}

/// Forward-auth (trusted edge proxy) config. The five fields fully describe
/// how to verify the proxy's signed assertion. GCP IAP preset:
/// `header = X-Goog-IAP-JWT-Assertion`,
/// `jwks_url = https://www.gstatic.com/iap/verify/public_key-jwk`,
/// `issuer = https://cloud.google.com/iap`,
/// `audience = /projects/<num>/global/backendServices/<id>`,
/// `email_claim = email`.
#[derive(Clone, Debug)]
pub struct ForwardAuthConfig {
    pub header: String,
    pub jwks_url: String,
    pub issuer: Option<String>,
    pub audience: Option<String>,
    pub email_claim: String,
}

/// Everything the auth layer needs, assembled by the coordinator from CLI/env.
#[derive(Clone, Debug)]
pub struct AuthConfig {
    pub mode: AuthMode,
    pub oidc: Option<OidcConfig>,
    pub forward_auth: Option<ForwardAuthConfig>,
    /// Deployment bearer tokens (the existing `--auth-tokens`).
    pub service_tokens: Vec<String>,
    /// Identity stamped for machine-created sessions / git attribution.
    pub service_email: String,
    /// Emails promoted to admin on JIT upsert.
    pub bootstrap_admins: Vec<String>,
    /// Synthetic-admin (dev) committer/display email.
    pub dev_default_email: String,
    /// Web-session cookie lifetime.
    pub session_ttl_hours: i64,
    /// Set the cookie `Secure` flag (off for http-localhost dev).
    pub cookie_secure: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        // No SSO → synthetic admin. This is the zero-config dev/test default,
        // so a coordinator (or a test harness) built without auth flags runs
        // as a single local admin.
        Self {
            mode: AuthMode::None,
            oidc: None,
            forward_auth: None,
            service_tokens: Vec::new(),
            service_email: "service@engram.local".to_string(),
            bootstrap_admins: Vec::new(),
            dev_default_email: "dev@engram.local".to_string(),
            session_ttl_hours: 24 * 7,
            cookie_secure: false,
        }
    }
}

/// Assemble the per-request verifier chain from config. Order (ADR 0031):
/// cookie session → service bearer → forward-auth → synthetic admin. The
/// OIDC authenticator is **not** in the chain — it's endpoint-driven
/// (`/auth/login` + `/auth/callback`) and persists via the session cookie,
/// which the cookie verifier then resolves.
pub fn build_chain(
    cfg: &AuthConfig,
    users: Arc<dyn UserStore>,
    web_sessions: Arc<dyn WebSessionStore>,
) -> VerifierChain {
    let mut verifiers: Vec<Box<dyn IdentityVerifier>> = Vec::new();

    // 1. Cookie session (resolves OIDC-minted human sessions). Harmless in
    //    forward-auth/none modes — it just misses when no cookie is present.
    verifiers.push(Box::new(CookieSession::new(web_sessions)));

    // 2. Service bearer (host-agent / CLI / machines), if configured.
    if !cfg.service_tokens.is_empty() {
        verifiers.push(Box::new(ServiceBearer::new(
            cfg.service_tokens.clone(),
            cfg.service_email.clone(),
        )));
    }

    // 3. Forward-auth, if behind a trusted proxy.
    if cfg.mode == AuthMode::ForwardAuth {
        if let Some(fa) = &cfg.forward_auth {
            verifiers.push(Box::new(ForwardAuthVerifier::new(fa.clone())));
        }
    }

    // 4. Synthetic admin only when no SSO is configured.
    if cfg.mode == AuthMode::None {
        verifiers.push(Box::new(SyntheticAdmin::new(cfg.dev_default_email.clone())));
    }

    // The service-bearer and synthetic-admin identities resolve to real
    // `users` rows via JIT; fold their emails into the admin allowlist so
    // they're provisioned as admin (machine callers + the dev admin).
    let mut admins = cfg.bootstrap_admins.clone();
    admins.push(cfg.service_email.clone());
    if cfg.mode == AuthMode::None {
        admins.push(cfg.dev_default_email.clone());
    }

    VerifierChain::new(verifiers, users, admins)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_mode_wire_round_trip() {
        for m in [AuthMode::Oidc, AuthMode::ForwardAuth, AuthMode::None] {
            assert_eq!(AuthMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(AuthMode::parse("forward_auth"), Some(AuthMode::ForwardAuth));
        assert_eq!(AuthMode::parse("bogus"), None);
    }
}
