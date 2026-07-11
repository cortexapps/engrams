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

/// ADR 0080 §C: RESOLVED static basic-auth registry creds, shipped
/// coord→host inside a `MaterializeImage` request (bincode in
/// `registry_auth_bincode`). The coordinator decrypts a `Static` row
/// via `CredCipher` and sends plain user/pass for the one pull; `None`
/// on the wire means "anonymous, or resolve host-side via the host's
/// ambient resolver" (GCP workload identity / public registries).
/// Never persisted host-side.
#[derive(Clone, Serialize, Deserialize)]
pub struct ResolvedRegistryAuth {
    pub username: String,
    pub password: String,
}

// Manual Debug: the password is a live secret — never let a stray
// `{:?}` (tracing, error context) leak it into logs.
impl std::fmt::Debug for ResolvedRegistryAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedRegistryAuth")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
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

/// One row in `enabled_images`. ADR 0080: the per-image config is
/// supplied out-of-band via the ImageService (EnableImage/UpdateImage)
/// and stored here as JSONB — the artifact carries no metadata — so
/// session-create has zero network dependency on any config path; the
/// host-agent still pulls the rootfs blob, but that's lazy and cached
/// separately by digest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnabledImage {
    pub id: Uuid,
    /// Full OCI reference: `host[:port]/repo[/path]:tag`.
    pub image_uri: String,
    /// The admin-authored [`ImageConfig`] (ADR 0080): name, description,
    /// env, workdir, resources, and the whole warm block (command +
    /// capture env + capture egress). Set at EnableImage, replaced by
    /// UpdateImage; capture-affecting fields only become visible here
    /// once the recapture's enable job reaches `ready` (the job carries
    /// the pending config).
    pub image_config: crate::types::image::ImageConfig,
    /// Dockerfile-derived `ENV`/`WORKDIR` defaults extracted from the OCI
    /// image config blob at enable time, merged UNDER `image_config` by
    /// [`Self::effective_config`].
    #[serde(default)]
    pub oci_defaults: crate::types::image::OciRuntimeDefaults,
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
}

impl EnabledImage {
    /// The effective per-image config: the admin [`ImageConfig`] with the
    /// Dockerfile-derived defaults merged under it. Every runtime
    /// consumer (boot bundle, session create/resume, evacuation
    /// recovery, capture) reads through this.
    pub fn effective_config(&self) -> crate::types::image::ImageConfig {
        self.image_config.merged_with(&self.oci_defaults)
    }
}

/// Public summary view used by `ImageService.ListEnabledImages`. Carries
/// the full admin [`ImageConfig`] (refs only for warm secrets — never
/// resolved values) so the dashboard's edit form pre-fills everything.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnabledImageSummary {
    pub id: Uuid,
    pub image_uri: String,
    pub manifest_digest: String,
    /// The admin-authored config (ADR 0080). What the dashboard renders
    /// AND edits — `config.name` / `config.description` replace the
    /// retired lifted `manifest_name`/`manifest_description` fields.
    pub config: crate::types::image::ImageConfig,
    pub last_refreshed_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

impl From<EnabledImage> for EnabledImageSummary {
    fn from(row: EnabledImage) -> Self {
        Self {
            id: row.id,
            image_uri: row.image_uri,
            manifest_digest: row.manifest_digest,
            config: row.image_config,
            last_refreshed_at: row.last_refreshed_at,
            created_at: row.created_at,
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

/// ADR 0088: one host's in-flight enable work — the operator roll/drain
/// gates' input, surfaced per host on the fleet view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveEnableWork {
    /// `enable_jobs` rows live-materializing on this host: state =
    /// `materializing`, `materialize_host_id` = host, and a fresh claim
    /// (the host's ≤30 s keepalive frames renew `claimed_at`, so a dead
    /// stream ages out within the lease window — never gate on a ghost).
    pub materializes: u32,
    /// `capture_jobs` rows in a non-terminal stage bound to this host.
    /// No freshness filter: the enable scanner's stage deadlines already
    /// redrive-or-fail a stuck capture row.
    pub captures: u32,
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
    /// ADR 0080: the [`ImageConfig`](crate::types::image::ImageConfig)
    /// this enable/refresh/update runs under, carried from the triggering
    /// request (or inherited from the existing enabled-image row). The
    /// scanner stamps it onto the `EnabledImage` row only at `ready` —
    /// which is what keeps a capture-affecting UpdateImage invisible to
    /// session-create until the new base snapshot exists. The capture
    /// stage resolves `warm.env` refs and assembles the `warm.network`
    /// egress policy from it.
    pub image_config: crate::types::image::ImageConfig,
    /// Operator-requested bypass of base-snapshot reuse. Used by
    /// `RefreshImage(force_recapture = true)` when a breaking runtime/tooling
    /// change needs a new base snapshot even if the rootfs content and
    /// resources match an existing enabled image.
    #[serde(default)]
    pub force_recapture: bool,
    /// ADR 0036 amendment (issue #538): per-host prestage outcome map,
    /// written once at the end of the `Prestaging` stage —
    /// `{"<host-uuid>": {"outcome": "staged"|"timed_out"|"unschedulable",
    /// "waited_ms": <u64>}}`. `{}` before the stage runs (or for jobs that
    /// predate this column / never had an eligible staging fleet).
    #[serde(default = "default_prestage_hosts")]
    pub prestage_hosts: serde_json::Value,
    /// Issue #539: live/last capture progress, persisted from the
    /// streaming `BuildBaseSnapshot` RPC's `CaptureProgress` events (and,
    /// on a `[warm]`-hook failure, left in place by `record_enable_job_failure`
    /// so the failing stage + tail survive even a `WarmExecTransport` kill).
    /// `None` outside the `capturing` state (or before this migration ever
    /// wrote a value for the row).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_phase: Option<crate::types::CapturePhase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm_stage_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub warm_stages: Vec<crate::types::WarmStageRecord>,
    /// Rolling last 16 KiB of the `[warm]` hook's combined stdout+stderr —
    /// the diagnostic that used to require host-log access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tail: Option<String>,
    // ADR 0084 (c): the capture placement reservation moved OFF this row
    // and ONTO `capture_jobs` (budgets + host_id + waiting_since live
    // there now, released implicitly on a terminal stage). The #621
    // `enable_jobs` reservation columns are retired (migration 0099).
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
