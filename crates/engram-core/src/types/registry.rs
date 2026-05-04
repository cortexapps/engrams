//! Registry credential and harness pack records.
//!
//! Phase 5 introduces Docker-registry-backed image and harness
//! distribution. These types describe the Postgres rows that index
//! the registry world. They're held by the `MetadataStore` trait so
//! the coordinator can run multi-replica without disk state.
//!
//! # Polymorphic credentials (5b)
//!
//! Registry auth comes in two fundamentally different shapes:
//!
//! - **Static**: a username + password the user typed. Stored
//!   envelope-encrypted; resolver decrypts on each pull. Covers
//!   DockerHub, GHCR, Quay, Harbor, and GAR-with-service-account-
//!   JSON-key.
//! - **Cloud-IAM**: the credential isn't a string the user can
//!   paste — it's a *claim* about the runtime's identity, exchanged
//!   for a short-lived token on each pull. The user never types a
//!   password; the host-agent's ambient cloud identity is the
//!   credential. Covers GAR-with-Workload-Identity (today's first
//!   variant), and AWS instance role / cross-account assume role
//!   (designed-in for; not implemented in this round).
//!
//! [`RegistryAuthSpec`] is the variant; [`RegistryCredential`] wraps
//! it with row metadata (id, host, timestamps).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One row in `registry_credentials`. The `auth` field carries the
/// variant-specific payload; identity / decryption / token-fetch
/// happens at the resolver layer (`engram-oci-auth`), not here.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegistryCredential {
    pub id: Uuid,
    pub registry_host: String,
    pub auth: RegistryAuthSpec,
    pub created_at: DateTime<Utc>,
    pub updated_at: Option<DateTime<Utc>>,
}

/// Variant-discriminated auth spec.
///
/// `serde(tag = "kind")` matches the JSONB on-disk layout — the
/// `kind` field of `auth_config` is also stored as the typed
/// `auth_kind` column so we can `WHERE auth_kind = 'static'` without
/// JSON probing.
///
/// Adding a new variant is three-line work: add the variant here,
/// add an `AuthStrategy` impl in `engram-oci-auth`, extend the SQL
/// CHECK constraint. No schema migration.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RegistryAuthSpec {
    /// Static username + envelope-encrypted password. The DEK is
    /// wrapped by the deployment's KEK; the cipher fields are bytes
    /// (the row stores them base64-encoded inside the JSONB column,
    /// since JSONB can't hold raw `bytea`).
    Static {
        username: String,
        wrapped_dek: Vec<u8>,
        nonce: Vec<u8>,
        ciphertext: Vec<u8>,
        key_id: String,
    },
    /// GCP Workload Identity: ambient runtime identity (GKE, GCE,
    /// Cloud Run) is exchanged for a short-lived OAuth access token
    /// on each pull. No stored secret material.
    ///
    /// `impersonate_sa: Some(email)` chains identity through IAM
    /// Credentials API to a target service account — useful when
    /// Engram runs under one identity but needs to pull as another.
    GcpWorkloadIdentity {
        #[serde(default)]
        impersonate_sa: Option<String>,
    },
    /// Public registries that don't require any auth at all (Docker
    /// Hub public images, ghcr.io public, the local dev registry,
    /// etc.). Stored as a row primarily so the dashboard can list
    /// the host and the operator can later upgrade it to a `Static`
    /// or `GcpWorkloadIdentity` entry without losing the host name.
    Anonymous,
}

impl RegistryAuthSpec {
    /// Stable wire string used as the `auth_kind` column. Must match
    /// the `serde(rename_all)` rendering and the SQL CHECK constraint
    /// in migration 0011.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Static { .. } => "static",
            Self::GcpWorkloadIdentity { .. } => "gcp_workload_identity",
            Self::Anonymous => "anonymous",
        }
    }
}

/// Public-facing view: the encrypted payload + any future secret-
/// adjacent fields are omitted so API responses can render this
/// directly without leaking ciphertext.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegistryCredentialSummary {
    pub id: Uuid,
    pub registry_host: String,
    /// Wire string of `auth.kind()` — `"static"`, `"gcp_workload_identity"`, ...
    pub auth_kind: String,
    /// For `static`: the username (well-known, non-secret).
    /// For cloud-IAM kinds: the impersonation target if any, else None.
    /// Always non-secret either way.
    pub auth_principal: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: Option<DateTime<Utc>>,
}

impl From<RegistryCredential> for RegistryCredentialSummary {
    fn from(c: RegistryCredential) -> Self {
        let (auth_kind, auth_principal) = match &c.auth {
            RegistryAuthSpec::Static { username, .. } => ("static", Some(username.clone())),
            RegistryAuthSpec::GcpWorkloadIdentity { impersonate_sa } => {
                ("gcp_workload_identity", impersonate_sa.clone())
            }
            RegistryAuthSpec::Anonymous => ("anonymous", None),
        };
        Self {
            id: c.id,
            registry_host: c.registry_host,
            auth_kind: auth_kind.to_string(),
            auth_principal,
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

/// One row in `enabled_images`. The manifest content is fetched
/// from the registry at enable time and stored on the row, so
/// session-create has zero network dependency on the manifest path
/// — the host-agent still pulls the rootfs blob, but that's lazy
/// and cached separately by digest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnabledImage {
    pub id: Uuid,
    /// Full OCI reference: `host[:port]/repo[/path]:tag`.
    pub image_uri: String,
    /// Verbatim `manifest.toml` from the registry — parsed at use
    /// site rather than persisted as JSON, since the upstream
    /// `ImageManifest` carries `deny_unknown_fields` and we want
    /// the original byte-for-byte representation when refreshing.
    pub manifest_toml: String,
    /// `sha256:...` digest of the OCI manifest layer holding the
    /// toml. Used to short-circuit refresh: if the registry's tag
    /// still resolves to the same digest, the row is already current.
    pub manifest_digest: String,
    pub last_refreshed_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: Option<DateTime<Utc>>,
}

/// Public summary view used by `GET /api/enabled-images`. Strips
/// the raw `manifest_toml` blob (clients re-render via the parsed
/// `ImageManifest` fields they care about — name, description,
/// secret schemas).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnabledImageSummary {
    pub id: Uuid,
    pub image_uri: String,
    pub manifest_digest: String,
    /// Parsed manifest name/description/secret-schemas — what the
    /// dashboard renders. Decoupled from the raw toml so a
    /// hand-edited row that fails to parse can still report a
    /// fallback summary.
    pub manifest_name: Option<String>,
    pub manifest_description: Option<String>,
    pub last_refreshed_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

impl From<EnabledImage> for EnabledImageSummary {
    fn from(row: EnabledImage) -> Self {
        // Try to lift display fields out of the parsed manifest. A
        // parse failure here just leaves them None — the row stays
        // in the list so an operator can still disable it; the
        // dashboard falls back to rendering `image_uri` only.
        let manifest: Option<crate::types::ImageManifest> = toml::from_str(&row.manifest_toml).ok();
        let (manifest_name, manifest_description) = match manifest {
            Some(m) => (Some(m.name), m.description),
            None => (None, None),
        };
        Self {
            id: row.id,
            image_uri: row.image_uri,
            manifest_digest: row.manifest_digest,
            manifest_name,
            manifest_description,
            last_refreshed_at: row.last_refreshed_at,
            created_at: row.created_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_spec_serde_round_trip_static() {
        let spec = RegistryAuthSpec::Static {
            username: "_json_key".into(),
            wrapped_dek: vec![1, 2, 3],
            nonce: vec![4, 5, 6],
            ciphertext: vec![7, 8, 9],
            key_id: "env:KEK:v1".into(),
        };
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["kind"], "static");
        assert_eq!(json["username"], "_json_key");
        let back: RegistryAuthSpec = serde_json::from_value(json).unwrap();
        assert!(matches!(back, RegistryAuthSpec::Static { .. }));
        assert_eq!(spec.kind(), "static");
    }

    #[test]
    fn auth_spec_serde_round_trip_gcp_wi_with_impersonation() {
        let spec = RegistryAuthSpec::GcpWorkloadIdentity {
            impersonate_sa: Some("engram@p.iam.gserviceaccount.com".into()),
        };
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["kind"], "gcp_workload_identity");
        assert_eq!(json["impersonate_sa"], "engram@p.iam.gserviceaccount.com");
        let back: RegistryAuthSpec = serde_json::from_value(json).unwrap();
        assert!(matches!(back, RegistryAuthSpec::GcpWorkloadIdentity { .. }));
        assert_eq!(spec.kind(), "gcp_workload_identity");
    }

    #[test]
    fn auth_spec_serde_round_trip_gcp_wi_ambient() {
        // No `impersonate_sa` field → defaults to None.
        let json: serde_json::Value =
            serde_json::from_str(r#"{"kind":"gcp_workload_identity"}"#).unwrap();
        let spec: RegistryAuthSpec = serde_json::from_value(json).unwrap();
        match spec {
            RegistryAuthSpec::GcpWorkloadIdentity { impersonate_sa } => {
                assert_eq!(impersonate_sa, None);
            }
            _ => panic!("expected GcpWorkloadIdentity"),
        }
    }

    #[test]
    fn summary_redacts_cipher_for_static() {
        let cred = RegistryCredential {
            id: Uuid::new_v4(),
            registry_host: "gcr.io".into(),
            auth: RegistryAuthSpec::Static {
                username: "_json_key".into(),
                wrapped_dek: vec![0xab; 64],
                nonce: vec![0xcd; 12],
                ciphertext: vec![0xef; 256],
                key_id: "env:KEK:v1".into(),
            },
            created_at: Utc::now(),
            updated_at: None,
        };
        let summary = RegistryCredentialSummary::from(cred);
        let json = serde_json::to_string(&summary).unwrap();
        // Spot-check that none of the cipher bytes leak via base64
        // or any other rendering — the summary only carries
        // (id, host, kind, principal, timestamps).
        assert!(!json.contains("ciphertext"));
        assert!(!json.contains("wrapped_dek"));
        assert!(!json.contains("nonce"));
        assert_eq!(summary.auth_kind, "static");
        assert_eq!(summary.auth_principal.as_deref(), Some("_json_key"));
    }

    #[test]
    fn summary_for_gcp_wi_carries_impersonation_target() {
        let cred = RegistryCredential {
            id: Uuid::new_v4(),
            registry_host: "us-east1-docker.pkg.dev".into(),
            auth: RegistryAuthSpec::GcpWorkloadIdentity {
                impersonate_sa: Some("engram@p.iam.gserviceaccount.com".into()),
            },
            created_at: Utc::now(),
            updated_at: None,
        };
        let summary = RegistryCredentialSummary::from(cred);
        assert_eq!(summary.auth_kind, "gcp_workload_identity");
        assert_eq!(
            summary.auth_principal.as_deref(),
            Some("engram@p.iam.gserviceaccount.com")
        );
    }
}
