//! ADR 0106: provider-neutral OAuth subjects, sealed credentials, and flows.
//!
//! The coordinator treats subject ids and provider payloads as opaque. Only a
//! trusted provider driver may interpret plaintext payloads; list/status APIs
//! expose [`OAuthAccountMetadata`] and never an envelope or token.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::SessionId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthSubjectKind {
    User,
    Connector,
    Mcp,
}

impl OAuthSubjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Connector => "connector",
            Self::Mcp => "mcp",
        }
    }
}

impl std::str::FromStr for OAuthSubjectKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "user" => Ok(Self::User),
            "connector" => Ok(Self::Connector),
            "mcp" => Ok(Self::Mcp),
            _ => Err(format!("unknown OAuth subject kind {value:?}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OAuthCredentialKey {
    pub subject_kind: OAuthSubjectKind,
    pub subject_id: String,
    pub provider: String,
}

/// Durable session authorization. Contains no OAuth bytes and is safe to
/// persist with the rest of the session write-set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionOAuthBinding {
    pub session_id: SessionId,
    pub key: OAuthCredentialKey,
}

/// Provider-approved metadata safe to return to clients and logs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthAccountMetadata {
    /// Provider-stable identity used to reject account switching on refresh.
    /// This value is not rendered by the UI.
    pub account_id: String,
    pub display_name: Option<String>,
    pub plan_type: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedOAuthCredential {
    pub key: OAuthCredentialKey,
    pub wrapped_dek: Vec<u8>,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub key_id: String,
    pub metadata: OAuthAccountMetadata,
    pub version: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
    /// Access-token expiry duplicated outside the ciphertext for refresh
    /// scheduling and status derivation. `None` for non-expiring bundles.
    pub expires_at: Option<DateTime<Utc>>,
    /// Set when the provider terminally rejected a refresh; the sealed bundle
    /// stays intact and a fresh authorization flow clears it.
    pub broken_at: Option<DateTime<Utc>>,
    pub broken_reason: Option<String>,
}

impl SealedOAuthCredential {
    /// Derived, client-safe lifecycle status. `expired` means past
    /// `expires_at` and not yet repaired by refresh — with the refresh
    /// scanner running it indicates refresh has been failing transiently.
    pub fn status(&self, now: DateTime<Utc>) -> OAuthCredentialStatus {
        if self.revoked_at.is_some() {
            OAuthCredentialStatus::Revoked
        } else if self.broken_at.is_some() {
            OAuthCredentialStatus::Broken
        } else if self.expires_at.is_some_and(|at| at <= now) {
            OAuthCredentialStatus::Expired
        } else {
            OAuthCredentialStatus::Connected
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthCredentialStatus {
    Connected,
    Expired,
    Broken,
    Revoked,
}

impl OAuthCredentialStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Expired => "expired",
            Self::Broken => "broken",
            Self::Revoked => "revoked",
        }
    }
}

/// Envelope supplied to a CAS write. The store owns version/timestamps.
/// A successful write always clears broken/claim state: publishing a
/// validated bundle IS the repair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSealedOAuthCredential {
    pub key: OAuthCredentialKey,
    pub wrapped_dek: Vec<u8>,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub key_id: String,
    pub metadata: OAuthAccountMetadata,
    /// Access-token expiry of the sealed bundle, if the provider expires it.
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OAuthFlowStatus {
    Pending,
    Succeeded,
    Denied,
    Cancelled,
    Expired,
    OwnerLost,
    Failed,
}

impl OAuthFlowStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Denied => "denied",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
            Self::OwnerLost => "owner_lost",
            Self::Failed => "failed",
        }
    }

    pub fn is_terminal(self) -> bool {
        self != Self::Pending
    }
}

impl std::str::FromStr for OAuthFlowStatus {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "pending" => Ok(Self::Pending),
            "succeeded" => Ok(Self::Succeeded),
            "denied" => Ok(Self::Denied),
            "cancelled" => Ok(Self::Cancelled),
            "expired" => Ok(Self::Expired),
            "owner_lost" => Ok(Self::OwnerLost),
            "failed" => Ok(Self::Failed),
            _ => Err(format!("unknown OAuth flow status {value:?}")),
        }
    }
}

/// Persisted flow state. Verification URI/code and provider messages stay only
/// in the owning replica's bounded in-memory state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OAuthFlow {
    pub id: Uuid,
    pub key: OAuthCredentialKey,
    pub owner_replica: String,
    pub lease_expires_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub status: OAuthFlowStatus,
    pub error_code: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
