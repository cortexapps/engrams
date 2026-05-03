//! Registry credential and harness pack records.
//!
//! Phase 5 introduces Docker-registry-backed image and harness
//! distribution. These types describe the Postgres rows that index
//! the registry world. They're held by the `MetadataStore` trait so
//! the coordinator can run multi-replica without disk state.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One row in `registry_credentials`.
///
/// The password is *never* in this struct in plaintext — when read
/// from Postgres, the row carries the sealed envelope; callers wanting
/// the plaintext password go through `engram-crypto::CredCipher::open`.
/// When writing, callers seal the plaintext before constructing this
/// row.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegistryCredential {
    pub id: Uuid,
    pub registry_host: String,
    pub username: String,
    /// Per-row DEK wrapped by the active KEK. Opaque to callers.
    pub wrapped_dek: Vec<u8>,
    /// AES-GCM nonce for `ciphertext`. 12 bytes.
    pub nonce: Vec<u8>,
    /// AES-256-GCM ciphertext of the password (UTF-8 bytes).
    pub ciphertext: Vec<u8>,
    /// KEK identifier active at write time. When this drifts from the
    /// current KEK, the row is a candidate for lazy re-wrap.
    pub key_id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: Option<DateTime<Utc>>,
}

/// Public-facing view: the password (and its envelope) is omitted so
/// API responses can render this directly without leaking ciphertext.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegistryCredentialSummary {
    pub id: Uuid,
    pub registry_host: String,
    pub username: String,
    pub key_id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl From<RegistryCredential> for RegistryCredentialSummary {
    fn from(c: RegistryCredential) -> Self {
        Self {
            id: c.id,
            registry_host: c.registry_host,
            username: c.username,
            key_id: c.key_id,
            created_at: c.created_at,
            updated_at: c.updated_at,
        }
    }
}

/// One row in `harness_packs`. Pointer-only — actual pack bytes live
/// in the registry at `registry_uri`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HarnessPack {
    pub id: Uuid,
    pub name: String,
    pub registry_uri: String,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: Option<DateTime<Utc>>,
}
