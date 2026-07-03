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

// ADR 0021 P1.5a retired `HarnessPack` + the `harness_packs` Postgres
// table — harnesses live in image rootfses now (built-ins injected by
// the baker from the catalog, custom by the author's Dockerfile), so
// there's no deployment-wide harness registry to model.

/// One row in `session_secrets`: the per-request `secrets` map a
/// dashboard / CLI client supplied at session-create time, sealed
/// under the deployment KEK so resume can put the same env back on
/// the post-resume harness child. The plaintext is a JSON-encoded
/// `HashMap<String, String>`; the ciphertext is AES-GCM, with the
/// DEK wrapped via the KEK (same shape as `registry_credentials`).
///
/// Lifetime: written once at session-create when overrides are
/// supplied, read once per resume, deleted via the table's
/// `ON DELETE CASCADE` on `sessions(id)`. The coordinator never
/// echoes the row in any list endpoint.
/// ADR 0047: the per-session credential-broker token, KEK-sealed (same
/// envelope shape as [`SessionSecrets`]). Minted exactly once per
/// session; any coordinator replica unseals it to authorize a guest
/// forge/upload request or to re-inject the env. Deleted at terminal
/// transition (plus the table's `ON DELETE CASCADE`).
#[derive(Clone, Debug)]
pub struct SessionBrokerToken {
    pub session_id: crate::SessionId,
    pub wrapped_dek: Vec<u8>,
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub key_id: String,
}

#[derive(Clone, Debug)]
pub struct SessionSecrets {
    pub session_id: crate::SessionId,
    pub wrapped_dek: Vec<u8>,
    /// 12-byte AES-GCM nonce, stored as `bytea`. The crypto crate's
    /// `SealedCred` types this as `[u8; 12]`; the trait surface uses
    /// `Vec<u8>` for trivial DB round-tripping and the conversion
    /// happens at the call sites.
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub key_id: String,
    pub created_at: DateTime<Utc>,
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
    /// ADR 0016 Phase C: the bake's chunked-disk `ManifestRef`,
    /// parsed from bundle.json at materialize-time. `None` for
    /// harness-only images (no chunked-disk artifact). Phase C's
    /// pin-set reads this directly to enumerate "the chunks every
    /// enabled image points at"; without persistence, the coord
    /// would have to re-pull bundle.json from OCI on every sweep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_manifest: Option<crate::types::manifest::ManifestRef>,
    /// ADR 0020 P1: the per-image base snapshot the create path restores
    /// from. The DB column is `NOT NULL` (an image is enabled IFF it has a
    /// base snapshot — migration 0038); this is `Option` only to mirror
    /// `disk_manifest`'s build-then-stamp flow (the enable handler captures
    /// the snapshot, then stamps this before the upsert). A persisted row
    /// always carries `Some`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_snapshot_id: Option<crate::types::ids::SnapshotId>,
    /// ADR 0021 P2: the base snapshot's *disk* manifest — the rootfs after
    /// template-boot, including the divergent runtime chunks the agent reads
    /// during startup (Bun / node_modules / claude). Denormalized from the
    /// snapshot record at enable so the heartbeat advertisement (and the
    /// host's residency prefetch) can warm it without a per-heartbeat
    /// snapshot lookup — the on-demand serial-from-GCS page-in of these chunks
    /// during `resume` is the substrate cost P2 retires. `NOT NULL` in the DB
    /// (migration 0042): residency requires it, so an image can't be enabled
    /// without it. Like `base_snapshot_id`, the `Option` here only mirrors the
    /// build-then-stamp shape — a persisted row always has `Some`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_snapshot_disk_manifest: Option<crate::types::manifest::ManifestRef>,
    /// ADR 0021 P2 (memory residency): the base snapshot's *memory* manifest —
    /// the chunked FC memory image the UFFD handler pages in at restore.
    /// Symmetric companion to `base_snapshot_disk_manifest`: denormalized from
    /// the snapshot record at enable so the heartbeat advertisement (and the
    /// host's residency prefetch) can warm these chunks on NVMe at host-boot,
    /// retiring the cold per-restore memory prefetch (~2.84 s on a freshly
    /// rolled host). `NOT NULL` in the DB (migration 0043); the `Option` only
    /// mirrors the build-then-stamp shape — a persisted row always has `Some`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_snapshot_memory_manifest: Option<crate::types::manifest::ManifestRef>,
    pub last_refreshed_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: Option<DateTime<Utc>>,
    /// ADR 0021 P1.8: soft-delete marker. `None` ⇒ live; the
    /// scheduler offers this image to new session-create. `Some(ts)`
    /// ⇒ disabled at `ts`; new sessions can't reference it but the
    /// resume path looks past the flag (so existing sessions can
    /// still come back). A future refcount-based chunk-GC reaps
    /// soft-deleted rows whose referencing sessions are all
    /// terminal.
    ///
    /// **Filter discipline**: every caller deciding "is this image
    /// available for a new session" must check `soft_deleted_at.is_none()`.
    /// Callers on the resume path (`resume_manifest_bundle`,
    /// evac-resumer's pipeline) intentionally do not, so a session
    /// whose image was disabled while it was idle can still resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_deleted_at: Option<DateTime<Utc>>,
    /// Capture-time environment for the image's `[warm]` hook (see
    /// [`CaptureEnvEntry`]). Set by the admin at enable/update time — NOT
    /// from the image manifest (ADR 0057: the image declares only what it
    /// *is*, not what a capture may hold). Resolved to values at
    /// base-snapshot capture and merged into the warm hook's exec env; these
    /// are *build/capture* secrets, distinct from a session's profile-injected
    /// runtime secrets. Stored as refs, never resolved values. Empty for an
    /// image with no `[warm]` hook (or one that needs no secrets).
    #[serde(default)]
    pub capture_env: Vec<CaptureEnvEntry>,
}

/// One capture-time environment entry for an image's `[warm]` hook. The
/// value is either a literal (a non-secret flag) or a secret ref resolved
/// at capture through the same [`crate::traits::SecretStore`] a session
/// uses (e.g. `gcp-sm://…`). Set on the *enable action*, persisted on the
/// [`EnabledImage`] row, and carried into a capture via the [`EnableJob`].
/// The coordinator stores the ref, resolves it transiently at capture, and
/// never logs or persists the resolved value.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaptureEnvEntry {
    /// Environment variable name the warm hook sees.
    pub name: String,
    pub value: CaptureEnvValue,
}

/// The value half of a [`CaptureEnvEntry`]: a literal, or a secret ref.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaptureEnvValue {
    /// A literal, non-secret value (a flag, a host name).
    Literal { value: String },
    /// A secret ref resolved at capture via the `SecretStore`. The ref is
    /// what's persisted; the resolved value is transient.
    SecretRef { secret_ref: String },
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
    /// Capture-time env attached to this enabled image (refs, never resolved
    /// values) — lets the dashboard's edit form pre-fill the current set.
    #[serde(default)]
    pub capture_env: Vec<CaptureEnvEntry>,
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
            capture_env: row.capture_env,
        }
    }
}

/// ADR 0036: state of an async image-enable job. The coordinator's
/// `enable_scanner` drives `Pending → Materializing → Capturing →
/// Prestaging → Ready`, with `Failed` as the give-up terminal after its
/// retry budget. See migration 0052 (+ 0081 for `Prestaging`, issue #538).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnableJobState {
    /// Recorded by `POST /api/enabled-images`; not yet picked up.
    Pending,
    /// Scanner is verifying/fetching chunks into BlobStorage
    /// (`chunks_done/chunks_total` advance during this state).
    Materializing,
    /// Chunks durable; capture VM boots + snapshots on a host.
    Capturing,
    /// ADR 0036 amendment (issue #538, INTERIM): the base snapshot is
    /// captured and advertised as a `prestage_images` heartbeat-ack entry;
    /// the scanner waits for every eligible (`stages_images`) host to
    /// report the digest in `ready_images`, or a deadline, before the
    /// `enabled_images` upsert makes the digest visible to session-create.
    Prestaging,
    /// Terminal: enabled_images row upserted; image usable.
    Ready,
    /// Terminal: retry budget exhausted; `error` says why. An admin
    /// retry re-queues to `Pending`.
    Failed,
}

impl EnableJobState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Materializing => "materializing",
            Self::Capturing => "capturing",
            Self::Prestaging => "prestaging",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Ready | Self::Failed)
    }
}

/// One row in `enable_jobs` (ADR 0036): an asynchronous image-enable
/// in flight (or terminal, kept for audit). The wire shape of
/// `GET /api/enable-jobs/:id` — `chunks_done/chunks_total` is the
/// operator-facing progress bar and the scanner's resume high-water
/// mark.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnableJob {
    pub id: Uuid,
    pub image_uri: String,
    /// OCI manifest digest observed at POST time (informational —
    /// the scanner re-pulls at materialize time).
    pub manifest_digest: Option<String>,
    pub state: EnableJobState,
    /// Total chunks in the image's bootstrap; `None` until the
    /// scanner has pulled + parsed the artifact metadata.
    pub chunks_total: Option<u32>,
    pub chunks_done: u32,
    /// Pipeline failures so far; the scanner flips to `Failed` once
    /// this exceeds its budget.
    pub attempts: u32,
    pub error: Option<String>,
    /// Capture-time env for this enable's `[warm]` hook, carried from the
    /// triggering request (enable/update) or inherited from the existing
    /// enabled-image row (refresh). The scanner stamps it onto the
    /// `EnabledImage` row before capture; `capture_and_record_base_snapshot`
    /// resolves the refs and injects them into the warm hook. See
    /// [`CaptureEnvEntry`].
    #[serde(default)]
    pub capture_env: Vec<CaptureEnvEntry>,
    /// ADR 0036 amendment (issue #538): per-host prestage outcome map,
    /// written once at the end of the `Prestaging` stage —
    /// `{"<host-uuid>": {"outcome": "staged"|"timed_out"|"unschedulable",
    /// "waited_ms": <u64>}}`. `{}` before the stage runs (or for jobs that
    /// predate this column / never had an eligible staging fleet).
    #[serde(default = "default_prestage_hosts")]
    pub prestage_hosts: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn default_prestage_hosts() -> serde_json::Value {
    serde_json::json!({})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_env_entry_jsonb_shape_round_trips() {
        // The JSONB shape persisted on enabled_images/enable_jobs. A tagged
        // `kind` discriminates literal vs secret_ref so the value is never
        // ambiguous.
        let entries = vec![
            CaptureEnvEntry {
                name: "FLAG".into(),
                value: CaptureEnvValue::Literal {
                    value: "true".into(),
                },
            },
            CaptureEnvEntry {
                name: "OP_TOKEN".into(),
                value: CaptureEnvValue::SecretRef {
                    secret_ref: "gcp-sm://p/secrets/op/versions/latest".into(),
                },
            },
        ];
        let json = serde_json::to_value(&entries).unwrap();
        assert_eq!(json[0]["name"], "FLAG");
        assert_eq!(json[0]["value"]["kind"], "literal");
        assert_eq!(json[0]["value"]["value"], "true");
        assert_eq!(json[1]["value"]["kind"], "secret_ref");
        assert_eq!(
            json[1]["value"]["secret_ref"],
            "gcp-sm://p/secrets/op/versions/latest"
        );
        let back: Vec<CaptureEnvEntry> = serde_json::from_value(json).unwrap();
        assert_eq!(back, entries);
    }

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
