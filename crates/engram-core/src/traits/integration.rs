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

use std::collections::HashMap;
use std::sync::Arc;

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

/// What a credential is being minted for.
///
/// `served_host` is the provider's OWN host identity (e.g. `github.com` — the
/// git host), letting a multi-host provider reject a request meant for a host it
/// doesn't serve. It is **not** the API endpoint the credential is used against
/// (e.g. `api.github.com`) — those are different namespaces. Only the git-forge
/// seam sets it (to the git remote host); every other caller — the egress broker
/// and the connector "test connection" probe — passes `None`. `owner` selects the
/// installation/org/user.
#[derive(Clone, Debug, Default)]
pub struct CredentialHint {
    pub served_host: Option<String>,
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

// ---------------------------------------------------------------------------
// Mint-kind registry (ADR 0057 C2) — the data-driven frame around Plane-A mint
// ---------------------------------------------------------------------------

/// The input kind of a mint-config field — drives the Plane-A admin form widget
/// and whether the value is sealed at rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MintFieldKind {
    /// Non-sensitive configuration (e.g. an App ID) — plain text input.
    Config,
    /// A credential (e.g. a private-key PEM) — masked input, stored sealed in
    /// the org-secret store.
    SealedSecret,
}

/// One field an admin supplies to configure a mint kind. `name` is also the
/// suffix of the org secret the coordinator resolves the value from at mint time
/// (`<kind>.<name>`), so the Plane-A write and the mint-time read agree on the
/// key without a side channel.
#[derive(Clone, Debug)]
pub struct MintFieldSchema {
    pub name: &'static str,
    pub label: &'static str,
    pub field_kind: MintFieldKind,
    pub required: bool,
}

/// Resolved field values (field `name` → value) used to build a mint engine.
pub type ResolvedFields = HashMap<String, String>;

/// Describes one mint kind (Plane A): the provider it backs, the form fields an
/// admin supplies, and how to build the [`Integration`] engine from resolved
/// field values. Each built-in mint crate (e.g. `engram-git-github`) exports one;
/// the coordinator assembles them into a registry it surfaces over gRPC
/// (`ListMintKinds`, C3) so the admin form is data-driven. The mint *logic* stays
/// bespoke Rust — this is the frame around it, not a generic config-driven mint.
#[derive(Clone)]
pub struct MintKindDescriptor {
    /// Stable kind id matching a connector's `credential.mint.kind` (e.g. `"github_app"`).
    pub kind: &'static str,
    /// The provider this mint kind serves (e.g. `"github"`).
    pub provider: &'static str,
    /// Human-readable label for the admin UI.
    pub display_name: &'static str,
    /// The config fields an admin fills in (also the org-secret name suffixes).
    pub fields: Vec<MintFieldSchema>,
    /// Build the engine from resolved field values. Pure (no I/O) — the
    /// coordinator resolves the values first.
    pub build: fn(&ResolvedFields) -> Result<Arc<dyn Integration>, IntegrationError>,
}
