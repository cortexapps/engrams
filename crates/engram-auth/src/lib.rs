//! Pluggable authentication for Engram (ADR 0031).
//!
//! ADR 0039 Task 32: the coordinator no longer serves human web traffic.
//! Only the service-bearer and forward-auth mechanisms remain in the chain.
//!
//! - [`ServiceBearer`] — deployment bearer token → admin-equivalent service
//!   principal. Per-request chain verifier (host-agent / CLI / machines).
//! - [`ForwardAuthVerifier`] — verifies a signed JWT forwarded by a trusted
//!   edge proxy (GCP IAP preset). Per-request chain verifier.
//! - [`OidcAuthenticator`] — OSS OIDC Authorization-Code + PKCE. Retained for
//!   the orchestrator's IAP bridge (Task 30); no longer used by the coordinator.
//!
//! Deleted in Task 32: `CookieSession` (web_sessions table dropped),
//! `SyntheticAdmin` (no human auth), `hash_token`, `mint_session_token`
//! (web session minting), `open_user_token`/`seal_user_token` (user token
//! storage removed in Task 31).

pub mod bearer;
pub mod chain;
pub mod config;
pub mod error;
pub mod forward;
pub mod jwks;
pub mod oidc;
pub mod session;
pub mod token;
pub mod verify;

pub use bearer::ServiceBearer;
pub use chain::VerifierChain;
pub use config::{build_chain, AuthConfig, AuthMode, ForwardAuthConfig, OidcConfig};
pub use error::AuthError;
pub use forward::ForwardAuthVerifier;
pub use oidc::{OidcAuthenticator, OidcStart};
pub use verify::{IdentityVerifier, Verified, VerifiedEmail, VerifyInput};

use uuid::Uuid;

/// Sentinel user id for the synthetic dev admin. Not a real `users` row —
/// dev/test sessions stamp this into `sessions.user_id`.
pub const SYNTHETIC_USER_ID: Uuid = Uuid::nil();

/// Sentinel user id for the service-bearer principal (host-agent / CLI /
/// machine callers). Distinct from the synthetic admin so audit logs can
/// tell them apart.
pub const SERVICE_USER_ID: Uuid = Uuid::from_u128(1);
