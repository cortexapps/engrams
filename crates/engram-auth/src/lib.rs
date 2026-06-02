//! Pluggable authentication for Engram (ADR 0031).
//!
//! engrams authenticated requests against a single deployment-wide bearer
//! token before this. ADR 0031 introduces per-user identity, expressed the
//! same way every other provider seam in the codebase is — a trait with one
//! implementation per mechanism, selected by config, **not** a `match` over
//! modes. This crate is framework-free (no axum); the coordinator extracts a
//! [`VerifyInput`] from each request and feeds it to a [`VerifierChain`].
//!
//! Every mechanism reduces a request to the same normalized output: a
//! [`Principal`](engram_core::types::user::Principal). Authenticators that
//! carry a verified email (forward-auth, and the OIDC callback) JIT-upsert a
//! user row to resolve the principal; mechanisms that already know the
//! identity (cookie session, service bearer, synthetic admin) produce it
//! directly.
//!
//! - [`OidcAuthenticator`] — OSS default; runs the Authorization-Code + PKCE
//!   flow against any OIDC issuer. Used by the `/auth/login` + `/auth/callback`
//!   endpoints (not the per-request chain).
//! - [`ForwardAuthVerifier`] — verifies a signed JWT forwarded by a trusted
//!   edge proxy. GCP IAP is a config preset of its five fields, not a code
//!   path. A per-request chain verifier.
//! - [`ServiceBearer`] — the existing deployment bearer token → an
//!   admin-equivalent service principal. A per-request chain verifier.
//! - [`SyntheticAdmin`] — when no SSO is configured, one built-in admin for
//!   every request so `just dev` runs with zero auth setup.
//! - [`CookieSession`] — resolves the HttpOnly session cookie via the
//!   [`WebSessionStore`](engram_core::traits::WebSessionStore).

pub mod bearer;
pub mod chain;
pub mod config;
pub mod cookie;
pub mod error;
pub mod forward;
pub mod jwks;
pub mod oidc;
pub mod session;
pub mod synthetic;
pub mod token;
pub mod verify;

pub use bearer::ServiceBearer;
pub use chain::VerifierChain;
pub use config::{build_chain, AuthConfig, AuthMode, ForwardAuthConfig, OidcConfig};
pub use cookie::CookieSession;
pub use error::AuthError;
pub use forward::ForwardAuthVerifier;
pub use oidc::{OidcAuthenticator, OidcStart};
pub use session::{hash_token, mint_session_token};
pub use synthetic::SyntheticAdmin;
pub use token::{open_user_token, seal_user_token};
pub use verify::{IdentityVerifier, Verified, VerifiedEmail, VerifyInput};

use uuid::Uuid;

/// Sentinel user id for the synthetic dev admin. Not a real `users` row —
/// dev/test sessions stamp this into `sessions.user_id`.
pub const SYNTHETIC_USER_ID: Uuid = Uuid::nil();

/// Sentinel user id for the service-bearer principal (host-agent / CLI /
/// machine callers). Distinct from the synthetic admin so audit logs can
/// tell them apart.
pub const SERVICE_USER_ID: Uuid = Uuid::from_u128(1);
