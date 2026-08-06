//! Byte-level golden + variant-index pins for the coord ↔ host-agent
//! gRPC `bytes`-payload wire types.
//!
//! ## Why this exists
//!
//! ADR 0013's host service carries structurally-complex payloads as
//! bincode blobs inside proto `bytes` fields
//! (`engram-protocol/src/grpc_client.rs::{encode,decode}_bincode`):
//! `SandboxSpec`, `AgentSpec`, `SnapshotMetadata`, `ManifestRef`,
//! `SessionEgressPolicy`, `CowState`/`CowStateRecord`. These are the
//! AUTHORITATIVE in-process engram-core types — they cross the wire
//! as-is, with no proto mirror per nested struct.
//!
//! `bincode` (1.x) is positional: enums encode by `u32` variant *index*,
//! structs by field *order*. Neither is self-describing. The existing
//! round-trip tests on these types cannot catch a format break — both
//! ends recompile together in CI, so a reordered field/variant or an
//! inserted (non-trailing) field round-trips green and then desyncs
//! against a peer built from an older tree. Coord and host roll on
//! independent (if close) schedules, so the skew window is real.
//!
//! This test pins the exact bytes (`golden/<name>.bin`) plus, for every
//! enum, the `u32` variant index in `bytes[0..4]`. Reordering a variant
//! or adding a non-trailing field fails here with a message naming the
//! evolution rule, turning a silent wire desync into a CI failure.
//!
//! Also covered: the `wire.rs` mirrors (`WireExecRequest`,
//! `WireReapStats`) — the pattern ADR 0013 says SHOULD eventually wrap
//! every cross-boundary shape.
//!
//! ## Regenerating the corpus (only when you INTENTIONALLY evolve a type)
//!
//! Adding a *trailing* enum variant or a *trailing* struct field is the
//! only wire-safe evolution. After such a change, regenerate:
//!
//! ```text
//!   cargo test -p engram-protocol --test wire_golden -- --ignored regen_golden
//! ```
//!
//! then `git add` the changed `golden/*.bin` and review the diff: an
//! EXISTING golden file changing bytes is a RED FLAG (you broke the wire
//! for a peer on the other side of a roll); only NEW files are expected.
//!
//! NOTE: every sample uses EMPTY or SINGLE-entry `HashMap`s and FIXED
//! UUIDs / timestamps so the encoding is deterministic.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use chrono::DateTime;
use engram_core::types::cow_state::{CowState, CowStateRecord};
use engram_core::types::egress::{
    EgressInjectEntry, EgressObserveEntry, EgressSecretEntry, SessionEgressPolicy,
};
use engram_core::types::image::{NetworkDefault, NetworkPolicy, SecretMode};
use engram_core::types::integration::{CredentialMintSource, MetadataFlavor};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::{
    AgentSpec, AuxBundleRef, AuxRoDrive, CpuLimit, DiskLimit, MemoryLimit, SandboxSpec,
};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::types::{WarmStageOutcome, WarmStageRecord};
use engram_core::{SandboxId, SessionId, SnapshotId};
use engram_protocol::wire::{
    WireExecRequest, WireReapStats, WireWriteFileResult, WireWriteFileSpec, WireWriteFilesRequest,
    WireWriteFilesResponse, WIRE_VERSION,
};
use serde::Serialize;
use uuid::Uuid;

fn golden_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden")
        .join(format!("{name}.bin"))
}

fn assert_golden<T>(name: &str, value: &T)
where
    T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let encoded = bincode::serialize(value).expect("bincode encode");
    let path = golden_path(name);
    let golden = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden file {}: {e}\n\
             regenerate with: cargo test -p engram-protocol --test wire_golden -- --ignored regen_golden",
            path.display()
        )
    });
    assert_eq!(
        encoded, golden,
        "wire format for `{name}` changed: bincode output != golden bytes.\n\
         bincode is POSITIONAL — a reordered field/variant or an inserted \
         (non-trailing) field breaks the coord↔host gRPC payload for a \
         peer on the other side of a deploy roll.\n\
         If this is an intentional, wire-SAFE evolution (a TRAILING \
         field/variant only), regenerate the corpus per the module header \
         and bump WIRE_VERSION."
    );
    let decoded: T = bincode::deserialize(&golden).expect("bincode decode golden");
    assert_eq!(
        &decoded, value,
        "golden bytes for `{name}` no longer decode to the expected value"
    );
}

fn assert_golden_no_eq<T: Serialize>(name: &str, value: &T) {
    // For types that aren't `PartialEq` (e.g. SandboxSpec). Pins the
    // encoded bytes; the round-trip-from-golden leg is covered by the
    // `PartialEq` variant for the types that have it.
    let encoded = bincode::serialize(value).expect("bincode encode");
    let path = golden_path(name);
    let golden = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden file {}: {e}\n\
             regenerate with: cargo test -p engram-protocol --test wire_golden -- --ignored regen_golden",
            path.display()
        )
    });
    assert_eq!(
        encoded, golden,
        "wire format for `{name}` changed: bincode output != golden bytes (see module header)."
    );
}

fn assert_variant_index<T: Serialize>(value: &T, idx: u32, variant: &str) {
    let encoded = bincode::serialize(value).expect("bincode encode");
    assert!(
        encoded.len() >= 4,
        "enum encoding too short for `{variant}`"
    );
    assert_eq!(
        &encoded[0..4],
        &idx.to_le_bytes(),
        "variant index for `{variant}` is not {idx}.\n\
         Enum variants crossing the bincode wire are APPEND-ONLY: a new \
         variant goes at the END so existing indices never shift. \
         Reordering/inserting desyncs a peer on the other side of a roll."
    );
}

// ---- fixed, deterministic constructors ---------------------------------

fn fixed_uuid(tag: u8) -> Uuid {
    Uuid::from_bytes([tag; 16])
}

fn fixed_manifest_ref(tag: u8, version: u64) -> ManifestRef {
    ManifestRef {
        manifest_id: fixed_uuid(tag),
        version,
    }
}

fn sandbox_spec() -> SandboxSpec {
    SandboxSpec {
        image: "warm-1".into(),
        rootfs_source: Some(PathBuf::from("/var/lib/engram/rootfs.ext4")),
        image_uri: Some("ghcr.io/cortexapps/engrams/dev:warm-1".into()),
        rootfs_manifest: Some(fixed_manifest_ref(0x10, 3)),
        cpu: CpuLimit { vcpus: 2 },
        memory: MemoryLimit { max_mib: 4096 },
        disk: DiskLimit { max_gib: 8 },
        ttl: None,
        env: HashMap::from([("ENGRAM_SESSION_ID".into(), "abc".into())]),
        workdir: Some("/workspace".into()),
        network: NetworkPolicy {
            default: NetworkDefault::Deny,
            allow_hosts: vec!["github.com".into()],
            allow_host_patterns: vec!["*.githubusercontent.com".into()],
        },
        aux_ro_drives: vec![AuxRoDrive {
            sha256: Some("a".repeat(64)),
            ..AuxRoDrive::reserved_slot(0)
        }],
        // ADR 0112 (wire v25): pin the `Some` encoding — the `None`
        // arm is a single 0 byte and is covered by the legacy-decode
        // test in engram-core.
        swap_mib: Some(6144),
    }
}

fn agent_spec() -> AgentSpec {
    AgentSpec {
        argv: vec!["/opt/engram/harness/harness".into()],
        env: HashMap::from([("ENGRAM_HARNESS_CWD".into(), "/workspace".into())]),
        session_env: HashMap::new(),
        host_ca_pem: Some("-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----\n".into()),
        // ADR 0073 (WIRE_VERSION bump, clean break): the attach-token
        // generation rides the spec.
        binding_epoch: 7,
    }
}

fn snapshot_metadata() -> SnapshotMetadata {
    SnapshotMetadata {
        id: SnapshotId::from(fixed_uuid(0x20)),
        size_bytes: 4096,
        created_at: DateTime::from_timestamp(1_770_000_000, 0).unwrap(),
        image_version: "warm-1".into(),
        disk_manifest: Some(fixed_manifest_ref(0x21, 1)),
        memory_manifest: Some(fixed_manifest_ref(0x22, 1)),
        base_memory_manifest: None,
        migration_source: None,
        source_sandbox_id: Some(SandboxId::from(fixed_uuid(0x23))),
        state_blob_key: Some("snapshots/x/state.bin".into()),
        sidecar_blob_key: None,
        rootfs_blob_key: None,
        working_set_blob_key: None,
        aux_bundles: vec![AuxBundleRef {
            drive_id: "skills".into(),
            sha256: "b".repeat(64),
        }],
        // v9: trailing field — the host's exact pause instant (issue #529).
        paused_at: Some(DateTime::from_timestamp(1_770_000_100, 0).unwrap()),
        peer_hints: Vec::new(),
    }
}

fn session_egress_policy() -> SessionEgressPolicy {
    SessionEgressPolicy {
        session_id: SessionId::from(fixed_uuid(0x30)),
        sandbox_id: SandboxId::from(fixed_uuid(0x31)),
        guest_ip: Ipv4Addr::new(169, 254, 0, 21),
        network_allow_hosts: vec!["github.com".into()],
        network_allow_host_patterns: vec!["*.amazonaws.com".into()],
        allow_all: false,
        secrets: vec![EgressSecretEntry {
            placeholder: "engram_ph_x".into(),
            real_value: "supersecret".into(),
            allow_hosts: vec!["api.anthropic.com".into()],
            allow_host_patterns: vec![],
        }],
        // ADR 0056: injects (Phase 3b) + observes (Phase 4b) ride
        // SessionEgressPolicy. Empty here — the golden pins their length
        // prefixes so a field reorder/removal is caught. WIRE_VERSION was
        // bumped for these additions (see wire.rs). ADR 0059 (v5) added GraphQL
        // fields to the inject/observe *entry* types; the golden is byte-identical
        // because an empty Vec encodes to a length prefix only (no element bytes).
        injects: vec![],
        observes: vec![],
        secret_mode: SecretMode::Broker,
        // ADR 0109: the host exposes metadata-style ADC only for sessions
        // whose immutable launch policy enables Google Cloud.
        metadata_flavor: Some(MetadataFlavor::Gce),
    }
}

/// The engrams-review outage (2026-08-01): every field of the inject/observe
/// ENTRY types populated — most importantly `mint_source: Some(..)`, which the
/// empty-`injects` golden above can never exercise. `CredentialMintSource` was
/// internally tagged (#931); bincode ENCODED that as a map but could never
/// DECODE it (`deserialize_any`), so every policy carrying a minted inject —
/// every GitHub PR-review session — failed host-side at boot. Wire v22 made it
/// externally tagged; this fixture pins the populated-entry encoding so the
/// entry types can't silently drop off the corpus again.
fn session_egress_policy_minted() -> SessionEgressPolicy {
    SessionEgressPolicy {
        injects: vec![EgressInjectEntry {
            secret: "ghs_minted".into(),
            header_name: "authorization".into(),
            header_template: "Bearer {}".into(),
            allow_hosts: vec!["api.github.com".into()],
            allow_host_patterns: vec![],
            methods: vec!["POST".into()],
            path_globs: vec!["/graphql".into()],
            graphql_operation: "mutation".into(),
            graphql_field: "createIssue".into(),
            mint_source: Some(CredentialMintSource::Connection {
                connection_id: "github-default".into(),
                provider: "github".into(),
            }),
            expires_at: Some(DateTime::from_timestamp(1_770_003_600, 0).unwrap()),
        }],
        observes: vec![EgressObserveEntry {
            allow_hosts: vec!["api.github.com".into()],
            allow_host_patterns: vec![],
            methods: vec!["POST".into()],
            path_globs: vec!["/repos/*/issues".into()],
            provider: "github".into(),
            asset_kind: "issue".into(),
            surface: "asset".into(),
            success_status_class: Some("2xx".into()),
            success_no_graphql_errors: false,
            graphql_operation: String::new(),
            graphql_field: String::new(),
            data: vec![("number".into(), "$.resp.number".into())],
            fetchable: Some("$.resp.html_url".into()),
            url_fallback: Some(engram_core::types::integration::ObserveUrlFallback {
                pattern: "https://github.com/{owner}/{name}/issues/{number:int}".into(),
                fields: vec![("number".into(), "{number}".into())],
            }),
        }],
        ..session_egress_policy()
    }
}

/// ADR 0109: a Google Cloud policy. Distinct from the GitHub fixture above in
/// the two ways that matter on this wire:
///
/// - the path glob carries the `segment-path:` prefix, which makes a `*` stop
///   at `/` and excludes the query string. Google clients append transport
///   parameters (`alt=json`, `pageSize`), while the permission boundary is the
///   resource path — a plain glob here would either over-match a nested action
///   or fail to match a real request. Nothing pinned that form before, so the
///   prefix could be dropped or renamed with no golden to notice.
/// - `mint_source` names the `gcp` provider, so the externally-tagged encoding
///   that #931 fixed stays pinned for a second provider.
fn session_egress_policy_google() -> SessionEgressPolicy {
    SessionEgressPolicy {
        network_allow_hosts: vec!["compute.googleapis.com".into()],
        injects: vec![EgressInjectEntry {
            secret: "ya29.minted".into(),
            header_name: "authorization".into(),
            header_template: "Bearer {}".into(),
            allow_hosts: vec!["monitoring.googleapis.com".into()],
            allow_host_patterns: vec![],
            methods: vec!["GET".into()],
            path_globs: vec!["segment-path:/v3/projects/*/timeSeries".into()],
            graphql_operation: String::new(),
            graphql_field: String::new(),
            mint_source: Some(CredentialMintSource::Connection {
                connection_id: "gcp-prod".into(),
                provider: "gcp".into(),
            }),
            expires_at: Some(DateTime::from_timestamp(1_770_003_600, 0).unwrap()),
        }],
        metadata_flavor: Some(MetadataFlavor::Gce),
        ..session_egress_policy()
    }
}

/// Wire v24 (ADR 0106 addendum): a policy whose inject rides the
/// `OauthConnector` mint source — a connector OAuth access token resolved
/// from the coordinator's sealed store. Unlike `Connection` mints the entry
/// keeps its REAL header template (`Bearer {}`) with the raw token as the
/// secret, so a refresh only supplies a new raw token. Pins the trailing
/// variant's encoding (index 1) inside a fully-populated entry.
fn session_egress_policy_oauth() -> SessionEgressPolicy {
    SessionEgressPolicy {
        injects: vec![EgressInjectEntry {
            secret: "linear-access-token".into(),
            header_name: "Authorization".into(),
            header_template: "Bearer {}".into(),
            allow_hosts: vec!["api.linear.app".into()],
            allow_host_patterns: vec![],
            methods: vec!["POST".into()],
            path_globs: vec!["/graphql".into()],
            graphql_operation: String::new(),
            graphql_field: String::new(),
            mint_source: Some(CredentialMintSource::OauthConnector {
                connection_id: "linear-default".into(),
                provider: "linear".into(),
            }),
            expires_at: Some(DateTime::from_timestamp(1_770_003_600, 0).unwrap()),
        }],
        ..session_egress_policy()
    }
}

fn cow_state() -> CowState {
    CowState {
        disk_manifest: fixed_manifest_ref(0x40, 7),
        dirty_chunks: 3,
        dirty_bytes: 48 * 1024 * 1024,
        last_flush_unix_ms: 1_770_000_000_000,
        base_chunks: 256,
        base_chunks_local: 240,
        memory_manifest: Some(fixed_manifest_ref(0x41, 2)),
        last_snapshot_unix_ms: 1_769_000_000_000,
    }
}

/// Issue #539 (historical): `Vec<WarmStageRecord>` used to cross the
/// coord<->host wire as the `CaptureProgress.warm_stages_bincode`
/// payload; ADR 0084 P1b deleted that RPC (and P4 deleted the
/// now-dead-code metadata verb, `update_enable_job_capture_progress`,
/// that used to write it), so this is now purely a JSON-in-JSONB
/// stability guard (`enable_jobs.warm_stages`) — kept pinned here
/// anyway since bincode encoding is a strictly harder guarantee than
/// JSON and this corpus already had the fixture. Two entries: one
/// CLOSED (`ended_at`
/// present, the `#[serde(skip_serializing_if)]` bincode-irrelevant but
/// exercised anyway) and one still OPEN (`ended_at: None`) — the shape a
/// failed or in-flight capture's stage history actually takes.
fn warm_stages() -> Vec<WarmStageRecord> {
    vec![
        WarmStageRecord {
            name: "deps-up".into(),
            started_at: DateTime::from_timestamp(1_770_000_000, 0).unwrap(),
            ended_at: Some(DateTime::from_timestamp(1_770_000_030, 0).unwrap()),
            outcome: WarmStageOutcome::Done,
        },
        WarmStageRecord {
            name: "migrations".into(),
            started_at: DateTime::from_timestamp(1_770_000_030, 0).unwrap(),
            ended_at: None,
            outcome: WarmStageOutcome::Running,
        },
    ]
}

// ---- tests -------------------------------------------------------------

#[test]
fn struct_payloads_golden() {
    // SandboxSpec / AgentSpec / SnapshotMetadata aren't `PartialEq`;
    // pin their encoded bytes (field-order regression catch).
    assert_golden_no_eq("sandbox_spec", &sandbox_spec());
    assert_golden_no_eq("agent_spec", &agent_spec());
    assert_golden_no_eq("snapshot_metadata", &snapshot_metadata());

    // These carry `PartialEq`, so also pin the decode-from-golden leg.
    assert_golden("manifest_ref", &fixed_manifest_ref(0x50, 9));
    assert_golden(
        "aux_ro_drive",
        &AuxRoDrive {
            sha256: Some("c".repeat(64)),
            ..AuxRoDrive::reserved_slot(1)
        },
    );
    assert_golden(
        "aux_bundle_ref",
        &AuxBundleRef {
            drive_id: "dyn_1".into(),
            sha256: "d".repeat(64),
        },
    );
    // SessionEgressPolicy / CowState / CowStateRecord don't derive
    // `PartialEq`; pin their encoded bytes (field-order regression catch).
    assert_golden_no_eq("session_egress_policy", &session_egress_policy());
    assert_golden_no_eq(
        "session_egress_policy_minted",
        &session_egress_policy_minted(),
    );
    assert_golden_no_eq(
        "session_egress_policy_google",
        &session_egress_policy_google(),
    );
    assert_golden_no_eq(
        "session_egress_policy_oauth",
        &session_egress_policy_oauth(),
    );
    assert_golden_no_eq("cow_state", &cow_state());
    assert_golden_no_eq(
        "cow_state_record",
        &CowStateRecord {
            sandbox_id: SandboxId::from(fixed_uuid(0x60)),
            state: cow_state(),
        },
    );
    // `Vec<WarmStageRecord>` — the `CaptureProgress.warm_stages_bincode`
    // payload (issue #539). `WarmStageRecord` derives `PartialEq`.
    assert_golden("warm_stages", &warm_stages());

    // ADR 0080 phase 3b (wire v14) — the MaterializeImage payloads.
    // `Option<ResolvedRegistryAuth>` crosses coord→host in
    // `registry_auth_bincode` (pin the `Some` shape; the empty-buffer
    // `None` never reaches bincode). `OciRuntimeDefaults` crosses
    // host→coord in `MaterializeImageDone.oci_defaults_bincode`
    // (single-entry map for determinism). Neither derives `PartialEq`
    // usefully here (ResolvedRegistryAuth has no `PartialEq`); pin bytes.
    assert_golden_no_eq(
        "resolved_registry_auth",
        &Some(engram_core::types::registry::ResolvedRegistryAuth {
            username: "robot$puller".into(),
            password: "hunter2".into(),
        }),
    );
    assert_golden_no_eq(
        "oci_runtime_defaults",
        &engram_core::types::image::OciRuntimeDefaults {
            env: HashMap::from([("PATH".to_string(), "/usr/bin".to_string())]),
            workdir: Some("/workspace".into()),
        },
    );
}

#[test]
fn nested_enum_variant_indices() {
    // SecretMode and NetworkDefault ride SessionEgressPolicy / SandboxSpec
    // on the wire. A reorder changes the encoded index — pin it.
    assert_golden("secret_mode_literal", &SecretMode::Literal);
    assert_golden("secret_mode_broker", &SecretMode::Broker);
    assert_variant_index(&SecretMode::Literal, 0, "SecretMode::Literal");
    assert_variant_index(&SecretMode::Broker, 1, "SecretMode::Broker");

    // CredentialMintSource rides EgressInjectEntry.mint_source on the wire
    // (v22, externally tagged — see the minted-policy fixture).
    assert_variant_index(
        &CredentialMintSource::Connection {
            connection_id: "github-default".into(),
            provider: "github".into(),
        },
        0,
        "CredentialMintSource::Connection",
    );
    assert_variant_index(
        &CredentialMintSource::OauthConnector {
            connection_id: "linear-default".into(),
            provider: "linear".into(),
        },
        1,
        "CredentialMintSource::OauthConnector",
    );

    assert_golden("network_default_allow", &NetworkDefault::Allow);
    assert_golden("network_default_deny", &NetworkDefault::Deny);
    assert_variant_index(&NetworkDefault::Allow, 0, "NetworkDefault::Allow");
    assert_variant_index(&NetworkDefault::Deny, 1, "NetworkDefault::Deny");

    // WarmStageOutcome rides WarmStageRecord (issue #539) on the wire.
    assert_golden("warm_stage_outcome_running", &WarmStageOutcome::Running);
    assert_golden("warm_stage_outcome_done", &WarmStageOutcome::Done);
    assert_golden("warm_stage_outcome_failed", &WarmStageOutcome::Failed);
    assert_variant_index(&WarmStageOutcome::Running, 0, "WarmStageOutcome::Running");
    assert_variant_index(&WarmStageOutcome::Done, 1, "WarmStageOutcome::Done");
    assert_variant_index(&WarmStageOutcome::Failed, 2, "WarmStageOutcome::Failed");
}

#[test]
fn wire_mirrors_golden() {
    // The wire.rs mirror shapes (ADR 0013 — the pattern the rest should
    // eventually adopt). WireExecRequest isn't `PartialEq`; pin bytes.
    assert_golden_no_eq(
        "wire_exec_request",
        &WireExecRequest {
            command: vec!["echo".into(), "hi".into()],
            stdin: Some(b"input".to_vec()),
            env: HashMap::from([("A".into(), "B".into())]),
            workdir: Some("/tmp".into()),
            timeout_ms: Some(5_500),
            exec_id: Some("exec-golden".into()),
            stdout_offset: Some(7),
            stderr_offset: Some(9),
            wake: Some(true),
        },
    );
    assert_golden_no_eq(
        "wire_reap_stats",
        &WireReapStats {
            files_scanned: 12,
            files_deleted: 5,
            bytes_freed: 1024,
            files_skipped_unparseable: 1,
            files_skipped_too_young: 2,
        },
    );
    // ADR 0100 (wire v17): the coord↔host WriteFiles payloads.
    assert_golden_no_eq("wire_write_files_request", &wire_write_files_request());
    assert_golden_no_eq("wire_write_files_response", &wire_write_files_response());
}

fn wire_write_files_request() -> WireWriteFilesRequest {
    WireWriteFilesRequest {
        files: vec![WireWriteFileSpec {
            path: "/workspace/.review/finder.md".into(),
            content: b"be skeptical".to_vec(),
            mode: Some(0o640),
        }],
    }
}

fn wire_write_files_response() -> WireWriteFilesResponse {
    WireWriteFilesResponse {
        results: vec![WireWriteFileResult {
            path: "/workspace/.review/finder.md".into(),
            ok: false,
            error: Some("read-only file system".into()),
        }],
    }
}

#[test]
fn wire_version_pinned() {
    // The version stamp coord/host exchange. Bumping it is the deliberate
    // signal that a payload shape changed; pin it so a payload change
    // without a bump (or vice-versa) is a conscious decision.
    //
    // 9 -> 10: ADR 0073 (epic #542) — bind_session + AgentSpec carry
    // binding_epoch, the shell-pin/rehandshake RPCs are deleted, heartbeat
    // gains harness_attached. Goldens regenerated in the same change.
    // 10 -> 11: ADR 0078 phase 2 (epic #548) — the heartbeat drops the
    // never-populated `local_snapshots` advert (placement affinity reads
    // PG `snapshots.host_id` instead). Goldens regenerated.
    // 11 -> 12: ADR 0079 (#543) — fencing_epoch on session-scoped host
    // RPCs (FencedSandboxRequest + the StartAgent/Restore pairs).
    // Proto-native fields only; bincode goldens unchanged.
    // 12 -> 13: ADR 0080 phase 2a — BuildBaseSnapshotRequest gains
    // capture_egress_bincode (coordinator-assembled capture egress);
    // WarmConfig (inside warm_bincode) gains `env`. Neither type is in
    // the golden corpus (SessionEgressPolicy itself is unchanged), so
    // bincode goldens are unchanged.
    // 13 -> 14: ADR 0080 phase 3b — the new server-streaming
    // `MaterializeImage` RPC (host-side materialization of standard
    // docker images). NEW bincode payloads only (`ResolvedRegistryAuth`
    // coord→host; `ManifestRef` — already pinned — and
    // `OciRuntimeDefaults` host→coord), goldens ADDED for the new
    // shapes; every existing golden is byte-identical.
    // 14 -> 15: ADR 0084 (#546) — capture_jobs heartbeat dispatch/
    // reporting. `Heartbeat.capture_job_reports` /
    // `HeartbeatAck.capture_assignments`/`acked_capture_jobs` ride the
    // JSON heartbeat/ack, NOT the gRPC bincode `bytes` payloads this
    // corpus pins — no new golden entries here. BuildBaseSnapshot RPC
    // deletion rides this bump too (removed in the cutover commit).
    // 15 -> 16: ADR 0095 — `SnapshotMetadata` gains the TRAILING
    // `peer_hints` field (peer-fill seed addrs for the restore
    // destination). Goldens regenerated. The `PeerChunkGet` RPC and the
    // JSON-ack `warm_peers` field ride this bump too (both are
    // independently roll-safe; the bump pins the deploy posture — a v16
    // coord never dispatches a peer-hinted restore to a v15 host).
    // 16 -> 17: ADR 0100 — the coord↔host WriteFiles RPC carries new
    // WireWriteFilesRequest/WireWriteFilesResponse bincode mirrors.
    // 17 -> 18: ADR 0093 addendum — `MaterializeImageRequest.min_disk_gib`
    // (the image's `suggested_disk_gib` now floors the packed ext4 size).
    // Proto-native scalar only; no bincode payload changed, so every
    // golden is byte-identical.
    // 19 -> 20: ADR 0103 review hardening — ExecFrame gains the proto-native
    // Refused terminal. No bincode payload changed.
    // 20 -> 21: ADR 0109 — `SessionEgressPolicy.google_adc` is a trailing
    // bincode field. The session-egress-policy golden was regenerated.
    // 21 -> 22: `CredentialMintSource` becomes externally tagged — the
    // internally-tagged form (#931) could never bincode-DECODE, so every
    // minted-inject policy failed host-side at boot (the engrams-review
    // outage, 2026-08-01). No existing golden changes bytes (the broken
    // `Some` shape never had one); the minted-policy golden is ADDED.
    // 22 -> 23: ADR 0109 seam — `SessionEgressPolicy.google_adc` becomes
    // `metadata_flavor: Option<MetadataFlavor>`. A field REPLACEMENT, not an
    // addition, so a v22 host cannot decode a v23 policy: the roll is
    // lockstep. All three session-egress-policy goldens were regenerated, and
    // the `session_egress_policy_google` fixture now pins the `Some(Gce)`
    // encoding beside the `None` case the other two carry.
    // 23 -> 24: ADR 0106 addendum — `CredentialMintSource` gains the TRAILING
    // `OauthConnector` variant (connector OAuth on the inject rail). Existing
    // goldens keep their bytes (trailing-variant addition); the oauth-policy
    // golden is ADDED and the variant index is pinned at 1.
    // 24 -> 25: ADR 0112 — `SandboxSpec` gains the TRAILING `swap_mib`
    // field (ephemeral guest swap size). Only `sandbox_spec.bin` changed
    // bytes (the fixture pins `Some(6144)`); every other golden embeds no
    // spec and kept its bytes. Lockstep coord+host roll.
    assert_eq!(
        WIRE_VERSION, 25,
        "WIRE_VERSION changed — confirm payload goldens were regenerated too"
    );
}

/// Writer for the golden corpus. `#[ignore]`d so a normal `cargo test`
/// never regenerates (which would mask a real break). See module header.
#[test]
#[ignore = "regenerates the golden corpus; run explicitly when intentionally evolving a wire type"]
fn regen_golden() {
    fn write<T: Serialize>(name: &str, value: &T) {
        let bytes = bincode::serialize(value).expect("bincode encode");
        let path = golden_path(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &bytes).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
        eprintln!("wrote {} ({} bytes)", path.display(), bytes.len());
    }

    write("sandbox_spec", &sandbox_spec());
    write("agent_spec", &agent_spec());
    write("snapshot_metadata", &snapshot_metadata());
    write("manifest_ref", &fixed_manifest_ref(0x50, 9));
    write(
        "aux_ro_drive",
        &AuxRoDrive {
            sha256: Some("c".repeat(64)),
            ..AuxRoDrive::reserved_slot(1)
        },
    );
    write(
        "aux_bundle_ref",
        &AuxBundleRef {
            drive_id: "dyn_1".into(),
            sha256: "d".repeat(64),
        },
    );
    write("session_egress_policy", &session_egress_policy());
    write(
        "session_egress_policy_minted",
        &session_egress_policy_minted(),
    );
    write(
        "session_egress_policy_google",
        &session_egress_policy_google(),
    );
    write(
        "session_egress_policy_oauth",
        &session_egress_policy_oauth(),
    );
    write("cow_state", &cow_state());
    write(
        "cow_state_record",
        &CowStateRecord {
            sandbox_id: SandboxId::from(fixed_uuid(0x60)),
            state: cow_state(),
        },
    );
    write("warm_stages", &warm_stages());
    // ADR 0080 phase 3b — MaterializeImage payloads (see
    // struct_payloads_golden for the shapes' rationale).
    write(
        "resolved_registry_auth",
        &Some(engram_core::types::registry::ResolvedRegistryAuth {
            username: "robot$puller".into(),
            password: "hunter2".into(),
        }),
    );
    write(
        "oci_runtime_defaults",
        &engram_core::types::image::OciRuntimeDefaults {
            env: HashMap::from([("PATH".to_string(), "/usr/bin".to_string())]),
            workdir: Some("/workspace".into()),
        },
    );

    write("secret_mode_literal", &SecretMode::Literal);
    write("secret_mode_broker", &SecretMode::Broker);
    write("warm_stage_outcome_running", &WarmStageOutcome::Running);
    write("warm_stage_outcome_done", &WarmStageOutcome::Done);
    write("warm_stage_outcome_failed", &WarmStageOutcome::Failed);
    write("network_default_allow", &NetworkDefault::Allow);
    write("network_default_deny", &NetworkDefault::Deny);

    write(
        "wire_exec_request",
        &WireExecRequest {
            command: vec!["echo".into(), "hi".into()],
            stdin: Some(b"input".to_vec()),
            env: HashMap::from([("A".into(), "B".into())]),
            workdir: Some("/tmp".into()),
            timeout_ms: Some(5_500),
            exec_id: Some("exec-golden".into()),
            stdout_offset: Some(7),
            stderr_offset: Some(9),
            wake: Some(true),
        },
    );
    write(
        "wire_reap_stats",
        &WireReapStats {
            files_scanned: 12,
            files_deleted: 5,
            bytes_freed: 1024,
            files_skipped_unparseable: 1,
            files_skipped_too_young: 2,
        },
    );
    write("wire_write_files_request", &wire_write_files_request());
    write("wire_write_files_response", &wire_write_files_response());
}

/// The engrams-review outage (2026-08-01): the internally-tagged
/// `CredentialMintSource` (#931) bincode-ENCODED fine but could never DECODE
/// (`deserialize_any`), so the encode-only golden pin above was green while
/// every minted-inject policy failed host-side. This test pins the DECODE leg:
/// a populated `Some(mint_source)` + `Some(expires_at)` must round-trip.
/// `SessionEgressPolicy` has no `PartialEq`, so assert the fields that carry
/// the regression.
#[test]
fn minted_inject_policy_bincode_round_trips() {
    let policy = session_egress_policy_minted();
    let bytes = bincode::serialize(&policy).expect("bincode encode minted policy");
    let back: SessionEgressPolicy =
        bincode::deserialize(&bytes).expect("bincode decode minted policy");
    assert_eq!(back.injects.len(), 1);
    assert_eq!(back.injects[0].mint_source, policy.injects[0].mint_source);
    assert_eq!(back.injects[0].expires_at, policy.injects[0].expires_at);
    assert_eq!(back.observes.len(), 1);
    assert_eq!(
        back.observes[0].url_fallback,
        policy.observes[0].url_fallback
    );
}

/// ADR 0080 prod regression (dev-brain enable): `WarmConfig.env` holds
/// `CaptureEnvValue`, an internally-tagged serde enum — bincode cannot
/// DECODE that representation (`deserialize_any`), so a non-empty
/// `[[warm.env]]` crossing the wire fails host-side at capture. The
/// coordinator therefore ships a STRIPPED wire clone (env cleared,
/// network dropped — both are coordinator concerns per the wire-v13
/// contract). This test pins both halves: the stripped shape
/// round-trips, and the tagged shape still fails decode — if a future
/// serde change makes the tagged enum bincode-safe, the second assert
/// fires and the strip (plus this test) can be retired.
#[test]
fn warm_config_wire_shape_round_trips_only_when_stripped() {
    use engram_core::types::image::{CaptureEnvEntry, CaptureEnvValue, WarmConfig};

    let stripped = WarmConfig {
        command: vec!["bash".into(), "-lc".into(), "/opt/engram/warm.sh".into()],
        timeout_secs: Some(3300),
        workdir: Some("/workspace".into()),
        env: Vec::new(),
        network: None,
    };
    let bytes = bincode::serialize(&Some(stripped.clone())).expect("encode stripped");
    let back: Option<WarmConfig> = bincode::deserialize(&bytes).expect("decode stripped");
    assert_eq!(back.as_ref().map(|w| &w.command), Some(&stripped.command));

    let tagged = WarmConfig {
        env: vec![CaptureEnvEntry {
            name: "OP_SERVICE_ACCOUNT_TOKEN".into(),
            value: CaptureEnvValue::SecretRef {
                secret_ref: "gcp-sm://x".into(),
            },
        }],
        ..stripped
    };
    let bytes = bincode::serialize(&Some(tagged)).expect("tagged encode currently succeeds");
    let res: Result<Option<WarmConfig>, _> = bincode::deserialize(&bytes);
    assert!(
        res.is_err(),
        "tagged CaptureEnvValue became bincode-decodable — retire the coordinator's \
         wire-strip in enabled_images.rs and this pin together"
    );
}
