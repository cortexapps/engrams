//! `OidcAuthenticator` — the OSS default. Runs the OIDC Authorization-Code +
//! PKCE flow against any issuer (discovery + JWKS), so engrams is agnostic to
//! the identity provider (Rippling, Okta, Auth0, ...) — the issuer is pure
//! config. Driven by the `/auth/login` and `/auth/callback` endpoints, not
//! the per-request chain: after a successful callback the coordinator mints a
//! session cookie, which [`CookieSession`](crate::CookieSession) then resolves.

use std::sync::RwLock;
use std::time::Duration;

use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::config::OidcConfig;
use crate::error::AuthError;
use crate::jwks::{fetch_jwks, resolve_key, validate_claims};
use crate::session::mint_session_token;
use crate::verify::VerifiedEmail;

/// The subset of the OIDC discovery document we use.
#[derive(Clone, Debug, Deserialize)]
struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
}

/// The transient state minted at `/auth/login`, stashed in a short-lived
/// signed cookie and replayed at `/auth/callback` to bind the redirect to
/// this browser (CSRF + PKCE + nonce).
#[derive(Clone, Debug)]
pub struct OidcStart {
    pub authorize_url: String,
    pub state: String,
    pub pkce_verifier: String,
    pub nonce: String,
}

pub struct OidcAuthenticator {
    cfg: OidcConfig,
    http: reqwest::Client,
    discovery: RwLock<Option<Discovery>>,
}

impl OidcAuthenticator {
    pub fn new(cfg: OidcConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        Self {
            cfg,
            http,
            discovery: RwLock::new(None),
        }
    }

    async fn discovery(&self) -> Result<Discovery, AuthError> {
        if let Some(d) = self.discovery.read().unwrap().clone() {
            return Ok(d);
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.cfg.issuer.trim_end_matches('/')
        );
        let d: Discovery = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| AuthError::Http(format!("oidc discovery: {e}")))?
            .error_for_status()
            .map_err(|e| AuthError::Http(format!("oidc discovery: {e}")))?
            .json()
            .await
            .map_err(|e| AuthError::Http(format!("oidc discovery decode: {e}")))?;
        *self.discovery.write().unwrap() = Some(d.clone());
        Ok(d)
    }

    /// Build the authorize URL + mint the PKCE verifier, CSRF state, and nonce.
    pub async fn start(&self) -> Result<OidcStart, AuthError> {
        let d = self.discovery().await?;
        let state = mint_session_token();
        let nonce = mint_session_token();
        let pkce_verifier = mint_session_token(); // 43 base64url chars — valid PKCE length
        let challenge = pkce_challenge(&pkce_verifier);
        let scope = if self.cfg.scopes.is_empty() {
            "openid email profile".to_string()
        } else {
            self.cfg.scopes.join(" ")
        };

        let mut url = Url::parse(&d.authorization_endpoint)
            .map_err(|e| AuthError::Config(format!("authorization_endpoint: {e}")))?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.cfg.client_id)
            .append_pair("redirect_uri", &self.cfg.redirect_url)
            .append_pair("scope", &scope)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");

        Ok(OidcStart {
            authorize_url: url.to_string(),
            state,
            pkce_verifier,
            nonce,
        })
    }

    /// Exchange the authorization `code` (with the PKCE verifier) for tokens,
    /// then validate the ID token's signature, issuer, audience (= client id),
    /// expiry, and `nonce`. Returns the verified email for JIT upsert. The
    /// caller is responsible for having matched the returned `state` against
    /// the stashed `OidcStart::state` first.
    pub async fn callback(
        &self,
        code: &str,
        pkce_verifier: &str,
        expected_nonce: &str,
    ) -> Result<VerifiedEmail, AuthError> {
        let d = self.discovery().await?;
        let params = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", self.cfg.redirect_url.as_str()),
            ("client_id", self.cfg.client_id.as_str()),
            ("client_secret", self.cfg.client_secret.as_str()),
            ("code_verifier", pkce_verifier),
        ];
        let token: TokenResponse = self
            .http
            .post(&d.token_endpoint)
            .form(&params)
            .send()
            .await
            .map_err(|e| AuthError::Http(format!("oidc token exchange: {e}")))?
            .error_for_status()
            .map_err(|e| AuthError::Http(format!("oidc token exchange: {e}")))?
            .json()
            .await
            .map_err(|e| AuthError::Http(format!("oidc token decode: {e}")))?;

        let id_token = token
            .id_token
            .ok_or_else(|| AuthError::Verify("token response had no id_token".into()))?;

        let jwks = fetch_jwks(&self.http, &d.jwks_uri).await?;
        let (key, alg) = resolve_key(&jwks, &id_token)?;
        validate_claims(
            &id_token,
            &key,
            alg,
            Some(&self.cfg.issuer),
            Some(&self.cfg.client_id),
            "email",
            Some(expected_nonce),
        )
    }
}

/// PKCE S256 challenge: base64url(sha256(verifier)), no padding.
fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_stable_and_url_safe() {
        // RFC 7636 Appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
