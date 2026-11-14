//! Byte-level pins for host payloads and their nested enum indices.
//!
//! `golden/ordinary_v38` holds unchanged predecessor bytes. Ordinary policy
//! tests use the transport codec; the other policy goldens pin the full internal
//! type, including managed authority. Never regenerate the predecessor corpus.
//!
//! The ignored `regen_golden` test writes only the current internal corpus.
//! Field additions are not compatible merely because they are trailing: a
//! positional bincode reader must agree on the complete payload shape.
//! Samples use empty or single-entry maps and fixed IDs for deterministic bytes.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;

use chrono::DateTime;
use engram_core::types::egress::{
    EgressInjectEntry, EgressObserveEntry, EgressSecretEntry, SessionEgressPolicy,
};
use engram_core::types::image::{NetworkDefault, NetworkPolicy, SecretMode};
use engram_core::types::integration::{CredentialMintSource, GuestService};
use engram_core::types::manifest::ManifestRef;
use engram_core::types::sandbox::{
    AgentSpec, AuxBundleRef, AuxRoDrive, CpuLimit, DiskLimit, MemoryLimit, SandboxSpec,
};
use engram_core::types::snapshot::SnapshotMetadata;
use engram_core::types::{WarmStageOutcome, WarmStageRecord};
use engram_core::{SandboxId, SessionId, SnapshotId};
use engram_protocol::wire::{WireExecRequest, WIRE_VERSION};
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
         Review the transport contract before changing this golden."
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
        swap_source: Some(PathBuf::from("/dev/nbd1")),
        swap_manifest: Some(fixed_manifest_ref(0x40, 7)),
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
        swap_manifest: None,
        disk_manifest: Some(fixed_manifest_ref(0x21, 1)),
        memory_manifest: Some(fixed_manifest_ref(0x22, 1)),
        base_memory_manifest: None,
        base_disk_manifest: None,
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
        managed_deny: None,
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
        apps: Vec::new(),
        secret_mode: SecretMode::Broker,
        // ADR 0109: the host exposes metadata-style ADC only for sessions
        // whose immutable launch policy enables Google Cloud.
        guest_services: vec![GuestService::new("gcp.gce_metadata")],
        tunnels: vec![],
    }
}

/// A populated minted policy: every field of the inject/observe
/// ENTRY types populated — most importantly `mint_source: Some(..)`, which the
/// empty-`injects` golden above can never exercise. `CredentialMintSource` was
/// internally tagged (#931); bincode ENCODED that as a map but could never
/// DECODE it (`deserialize_any`), so every policy carrying a minted inject —
/// every GitHub PR-review session — failed host-side at boot. Wire v22 made it
/// externally tagged; this fixture pins the populated-entry encoding so the
/// entry types can't silently drop off the corpus again.
fn session_egress_policy_minted() -> SessionEgressPolicy {
    SessionEgressPolicy {
        managed_deny: None,
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
        managed_deny: None,
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
        guest_services: vec![GuestService::new("gcp.gce_metadata")],
        tunnels: vec![engram_core::types::integration::SessionTunnel {
            id: "prod-readonly".into(),
            connector: "gcp.cloud_sql".into(),
            config_json:
                r#"{"instance":"customer:us-central1:prod","database_user":"reader@customer.iam"}"#
                    .into(),
            mint_source: Some(CredentialMintSource::Connection {
                connection_id: "gcp-prod".into(),
                provider: "gcp".into(),
            }),
        }],
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
        managed_deny: None,
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

/// ADR 0115: `OauthUser` mint source — a user's personal connector
/// credential (OAuth or static token) resolved from the sealed store, the
/// subject stamped at compile time. Same raw-token/real-template contract as
/// `OauthConnector`. Pins the trailing variant's encoding (index 2) inside a
/// fully-populated entry.
fn session_egress_policy_oauth_user() -> SessionEgressPolicy {
    SessionEgressPolicy {
        managed_deny: None,
        injects: vec![EgressInjectEntry {
            secret: "personal-access-token".into(),
            header_name: "Authorization".into(),
            header_template: "Bearer {}".into(),
            allow_hosts: vec!["api.linear.app".into()],
            allow_host_patterns: vec![],
            methods: vec!["POST".into()],
            path_globs: vec!["/graphql".into()],
            graphql_operation: String::new(),
            graphql_field: String::new(),
            mint_source: Some(CredentialMintSource::OauthUser {
                user_id: "user-7".into(),
                connection_id: "linear-default".into(),
                provider: "linear".into(),
            }),
            expires_at: Some(DateTime::from_timestamp(1_770_003_600, 0).unwrap()),
        }],
        ..session_egress_policy()
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
fn migration_item_indices_stay_fixed() {
    use engram_core::types::snapshot::{DiskRole, MigrationItem};
    use engram_protocol::grpc::migration_item::Kind;
    for (item, index) in [
        (MigrationItem::StateBin, 0),
        (MigrationItem::DiskSealInfo(DiskRole::Root), 1),
        (MigrationItem::DiskBlockAt(DiskRole::Root, 9), 2),
        (MigrationItem::HotHint, 3),
        (MigrationItem::DiskPrecopyInfo(DiskRole::Root), 4),
    ] {
        // Exhaustive guard: every new kind needs an explicit wire pin.
        let kind = match item {
            MigrationItem::StateBin => Kind::StateBin,
            MigrationItem::DiskSealInfo(_) => Kind::DiskSealInfo,
            MigrationItem::DiskBlockAt(_, _) => Kind::DiskBlockAt,
            MigrationItem::HotHint => Kind::HotHint,
            MigrationItem::DiskPrecopyInfo(_) => Kind::DiskPrecopyInfo,
        };
        assert_variant_index(&item, index, "MigrationItem");
        assert_eq!(kind as i32, [1, 4, 7, 6, 8][index as usize]);
        let bytes = bincode::serialize(&item).unwrap();
        assert_eq!(bincode::deserialize::<MigrationItem>(&bytes).unwrap(), item);
    }
}

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
    // Pin the policy field order, including the optional managed deny.
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
    assert_golden_no_eq(
        "session_egress_policy_oauth_user",
        &session_egress_policy_oauth_user(),
    );
    // Pin the persisted warm-stage shape.
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
    assert_variant_index(
        &CredentialMintSource::OauthUser {
            user_id: "user-7".into(),
            connection_id: "linear-default".into(),
            provider: "linear".into(),
        },
        2,
        "CredentialMintSource::OauthUser",
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
    // Pin the exec request field order.
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
}

#[test]
fn wire_version_pinned() {
    assert_eq!(WIRE_VERSION, 38);
    assert_eq!(engram_protocol::wire::MANAGED_PROTOCOL_VERSION, 43);
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
    write("session_egress_policy_managed", &managed_egress_policy());
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
    write(
        "session_egress_policy_oauth_user",
        &session_egress_policy_oauth_user(),
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
}

/// Populated minted credentials must retain every field through internal encoding.
#[test]
fn minted_inject_policy_bincode_round_trips() {
    let policy = session_egress_policy_minted();
    let bytes = bincode::serialize(&policy).expect("bincode encode minted policy");
    let back: SessionEgressPolicy =
        bincode::deserialize(&bytes).expect("bincode decode minted policy");
    assert_eq!(back, policy);
}

/// ADR 0080 capture contract: `WarmConfig.env` holds
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

fn managed_egress_policy() -> SessionEgressPolicy {
    let mut policy = session_egress_policy();
    policy.managed_deny = Some(
        engram_core::types::managed_deny::ManagedDenyPolicy::new(
            9,
            ["mcp.example".into(), "alias.example".into()],
        )
        .unwrap(),
    );
    policy
}
#[test]
fn managed_policy_internal_encoding_retains_authority() {
    assert_golden("session_egress_policy_managed", &managed_egress_policy());
}

#[test]
fn ordinary_v38_source_bytes_are_unchanged() {
    for (name, policy) in [
        ("session_egress_policy", session_egress_policy()),
        (
            "session_egress_policy_minted",
            session_egress_policy_minted(),
        ),
        (
            "session_egress_policy_google",
            session_egress_policy_google(),
        ),
        ("session_egress_policy_oauth", session_egress_policy_oauth()),
        (
            "session_egress_policy_oauth_user",
            session_egress_policy_oauth_user(),
        ),
    ] {
        let old = std::fs::read(
            golden_path(name)
                .parent()
                .unwrap()
                .join("ordinary_v38")
                .join(format!("{name}.bin")),
        )
        .unwrap();
        assert_eq!(
            engram_protocol::ordinary_policy::encode(&policy).unwrap(),
            old,
            "{name}"
        );
        assert_eq!(
            engram_protocol::ordinary_policy::decode(&old).unwrap(),
            policy
        );
        for cut in 0..old.len() {
            assert!(engram_protocol::ordinary_policy::decode(&old[..cut]).is_err());
        }
        let mut trailing = old;
        trailing.push(0);
        assert!(engram_protocol::ordinary_policy::decode(&trailing).is_err());
    }
}

#[test]
fn ordinary_policy_rejects_managed_authority_even_when_empty() {
    for hosts in [vec![], vec!["mcp.example".to_string()]] {
        let mut policy = session_egress_policy();
        policy.managed_deny =
            Some(engram_core::types::managed_deny::ManagedDenyPolicy::new(1, hosts).unwrap());
        assert!(engram_protocol::ordinary_policy::encode(&policy).is_err());
    }
}

#[test]
fn ordinary_policy_preserves_apps_and_secret_modes() {
    for secret_mode in [SecretMode::Literal, SecretMode::Broker] {
        let mut policy = session_egress_policy_google();
        policy.apps.push(engram_core::types::egress::AppEndpoint {
            hostname: "app.example".into(),
            port: 8080,
        });
        policy.secret_mode = secret_mode;
        policy.allow_all = true;
        assert_eq!(
            engram_protocol::ordinary_policy::decode(
                &engram_protocol::ordinary_policy::encode(&policy).unwrap()
            )
            .unwrap(),
            policy
        );
    }
}
