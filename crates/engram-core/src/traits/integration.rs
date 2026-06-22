//! ADR 0056: the slim provider-integration seam — subsumes the retired
//! `GitForge`.
//!
//! An [`Integration`] is a *platform-side* authority living in the coordinator.
//! It holds the long-lived provider credential (e.g. a GitHub App private key)
//! and exposes the two irreducible per-provider operations that can't be pure
//! connector config (ADR 0056 §6):
//!
//! 1. [`Integration::mint_credential`] — credential source = **mint** (Plane A):
//!    mint a short-lived credential scoped to EXACTLY the session's bound
//!    capabilities. Inject-source providers don't mint (they are connector
//!    config); the default errors `Unsupported`.
//! 2. [`Integration::perform_action`] — the **hybrid mediated** action seam
//!    (§4): a server-performed effect for the rare pre-effect cases (opening a
//!    PR). Most side effects are *observed* at the interceptor instead; the
//!    default errors `Unsupported`.
//!
//! Gating, credential injection, and asset observation all live in the egress
//! interceptor + connector config — NOT here. This trait is only the bespoke,
//! security-critical residue.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::IntegrationError;
use crate::types::Capability;

/// A short-lived, capability-scoped credential the guest wields. Generalizes
/// the retired `ScopedToken`: a GitHub App installation token is the
/// `Basic { username: "x-access-token", password: "ghs_…" }` case. `expires_at`
/// is advisory for the coordinator's caching — the guest fetches fresh per op.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ScopedCredential {
    /// HTTP basic / git askpass (username + password).
    Basic {
        username: String,
        password: String,
        expires_at: DateTime<Utc>,
    },
    /// A bearer token (OAuth / GCP / RFC 8693 token-exchange).
    Bearer {
        token: String,
        expires_at: DateTime<Utc>,
    },
    /// AWS STS temporary credentials.
    AwsSts {
        access_key_id: String,
        secret_access_key: String,
        session_token: String,
        expires_at: DateTime<Utc>,
    },
}

/// What a credential is being minted for. `host` lets a multi-host provider
/// reject a mismatched request; `owner` selects the installation/org/user.
#[derive(Clone, Debug, Default)]
pub struct CredentialHint {
    pub host: Option<String>,
    pub owner: Option<String>,
}

#[async_trait]
pub trait Integration: Send + Sync {
    /// Stable provider id (e.g. `"github"`) — namespaces capabilities + assets.
    fn provider(&self) -> &str;

    /// Credential source = MINT (Plane A): mint a credential covering EXACTLY
    /// `caps` (already clamped server-side at create). Called on demand by the
    /// in-session forge seam; never injected at create. Impls cache + lazily
    /// refresh. Inject-source providers don't mint — default `Unsupported`.
    async fn mint_credential(
        &self,
        _caps: &[Capability],
        _hint: &CredentialHint,
    ) -> Result<ScopedCredential, IntegrationError> {
        Err(IntegrationError::Unsupported)
    }

    /// Hybrid mediated action (§4): a server-performed effect (e.g. open a PR).
    /// `args`/reply are JSON; the coordinator emits any resulting
    /// `IntegrationAsset` from the reply. Most actions are observed at the
    /// interceptor, not mediated here — default `Unsupported`.
    async fn perform_action(
        &self,
        _cap: &Capability,
        _args: &serde_json::Value,
    ) -> Result<serde_json::Value, IntegrationError> {
        Err(IntegrationError::Unsupported)
    }
}
