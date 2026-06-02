//! Auth error type. Hand-written enum per project convention (no thiserror).

use std::error::Error as StdError;
use std::fmt;

use engram_core::MetaError;

#[derive(Debug)]
pub enum AuthError {
    /// A user/web-session store call failed.
    Store(MetaError),
    /// The principal resolved but is deprovisioned (`active = false`).
    Inactive,
    /// A credential was present but failed verification (bad signature,
    /// wrong audience/issuer, expired, malformed claims). Distinct from
    /// "no credential" (which is `Ok(None)` from a verifier).
    Verify(String),
    /// Misconfiguration (missing OIDC issuer, unparseable URL, ...).
    Config(String),
    /// A network dependency failed (OIDC discovery, JWKS fetch, token
    /// exchange).
    Http(String),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(e) => write!(f, "auth store error: {e}"),
            Self::Inactive => write!(f, "user is deprovisioned"),
            Self::Verify(m) => write!(f, "credential verification failed: {m}"),
            Self::Config(m) => write!(f, "auth configuration error: {m}"),
            Self::Http(m) => write!(f, "auth network error: {m}"),
        }
    }
}

impl StdError for AuthError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Store(e) => Some(e),
            _ => None,
        }
    }
}

impl From<MetaError> for AuthError {
    fn from(e: MetaError) -> Self {
        Self::Store(e)
    }
}
